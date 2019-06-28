use std::{io, net, error, time};
use std::sync::Arc;
use std::net::SocketAddr;
use parking_lot::RwLock;
use futures::{Future, finished, failed};
use futures::stream::Stream;
use futures_cpupool::CpuPool;
use tokio_io::IoFuture;
use tokio_core::net::{TcpListener, TcpStream};
use tokio_core::reactor::{Handle, Remote, Timeout, Interval};
use abstract_ns::Resolver;
use ns_dns_tokio::DnsResolver;
use message::{Payload, MessageResult, Message};
use message::common::Services;
use message::types::addr::AddressEntry;
use net::{connect, Connections, Channel, Config as NetConfig, accept_connection, ConnectionCounter};
use util::{NodeTable, Node, NodeTableError, Direction};
use session::{SessionFactory, SeednodeSessionFactory, NormalSessionFactory};
use {Config, PeerId};
use protocol::{LocalSyncNodeRef, InboundSyncConnectionRef, OutboundSyncConnectionRef};
use io::DeadlineStatus;

pub type BoxedEmptyFuture = Box<Future<Item=(), Error=()> + Send>;

/// Network context.
pub struct Context {
	/// Connections.
	connections: Connections,
	/// Connection counter.
	connection_counter: ConnectionCounter,
	/// Node Table.
	node_table: RwLock<NodeTable>,
	/// Thread pool handle.
	pool: CpuPool,
	/// Remote event loop handle.
	remote: Remote,
	/// Local synchronization node.
	local_sync_node: LocalSyncNodeRef,
	/// Node table path.
	config: Config,
}

impl Context {
	/// Creates new context with reference to local sync node, thread pool and event loop.
	pub fn new(local_sync_node: LocalSyncNodeRef, pool_handle: CpuPool, remote: Remote, config: Config) -> Result<Self, Box<error::Error>> {
		let context = Context {
			connections: Default::default(),
			connection_counter: ConnectionCounter::new(config.inbound_connections, config.outbound_connections),
			node_table: RwLock::new(try!(NodeTable::from_file(config.preferable_services, &config.node_table_path))),
			pool: pool_handle,
			remote: remote,
			local_sync_node: local_sync_node,
			config: config,
		};

		Ok(context)
	}

	/// Spawns a future using thread pool and schedules execution of it with event loop handle.
	pub fn spawn<F>(&self, f: F) where F: Future + Send + 'static, F::Item: Send + 'static, F::Error: Send + 'static {
		let pool_work = self.pool.spawn(f);
		self.remote.spawn(move |_handle| {
			pool_work.then(|_| finished(()))
		})
	}

	/// Schedules execution of function in future.
	/// Use wisely, it keeps used objects in memory until after it is resolved.
	pub fn execute_after<F>(&self, duration: time::Duration, f: F) where F: FnOnce() + 'static + Send {
		let pool = self.pool.clone();
		self.remote.spawn(move |handle| {
			let timeout = Timeout::new(duration, handle)
				.expect("Expected to schedule timeout")
				.then(move |_| {
					f();
					finished(())
				});
			pool.spawn(timeout)
		});
	}

	/// Returns addresses of recently active nodes. Sorted and limited to 1000.
	pub fn node_table_entries(&self) -> Vec<Node> {
		self.node_table.read().recently_active_nodes(self.config.internet_protocol)
	}

	/// Updates node table.
	pub fn update_node_table(&self, nodes: Vec<AddressEntry>) {
		info!("Updating node table with {} entries", nodes.len());
		self.node_table.write().insert_many(nodes);
	}

	/// Penalize node.
	//惩罚节点？ 
	pub fn penalize_node(&self, addr: &SocketAddr) {
		info!("Penalizing node {}", addr);
		self.node_table.write().note_failure(addr);
	}

	/// Adds node to table.
	pub fn add_node(&self, addr: SocketAddr) -> Result<(), NodeTableError> {
		info!("Adding node {} to node table", &addr);
		self.node_table.write().add(addr, self.config.connection.services)
	}

	/// Removes node from table.
	pub fn remove_node(&self, addr: SocketAddr) -> Result<(), NodeTableError> {
		info!("Removing node {} from node table", &addr);
		self.node_table.write().remove(&addr)
	}

	/// Every 10 seconds check if we have reached maximum number of outbound connections.
	/// If not, connect to best peers.
	pub fn autoconnect(context: Arc<Context>, handle: &Handle) {
		let c = context.clone();
		// every 10 seconds connect to new peers (if needed)
		let interval: BoxedEmptyFuture = Box::new(Interval::new_at(time::Instant::now(), time::Duration::new(10, 0), handle).expect("Failed to create interval")
			.and_then(move |_| {
				// print traces
				let ic = context.connection_counter.inbound_connections();
				let oc = context.connection_counter.outbound_connections();
				info!("Inbound connections: ({}/{})", ic.0, ic.1);
				info!("Outbound connections: ({}/{})", oc.0, oc.1);
				//维护channel 里面的建立好的连接;针对每个seession,在间隔固定时间后，都需要去检查连接的可用性
				for channel in context.connections.channels().values() {
					channel.session().maintain();//最终实现，在protocol/ping.rs里面实现，但是只有看到发送ping消息，没有看到关于回复的处理，即更新存活时间
				}
				//判断当前还需要创建几个连接会话
				let needed = context.connection_counter.outbound_connections_needed() as usize;
				if needed != 0 {
					// TODO: pass Services::with_bitcoin_cash(true) after HF block  
					let used_addresses = context.connections.addresses();//获取当前已经保持连接的地址，用于在下一步获取新地址时，排除掉这些地址
					//返回所需数量的节点地址
					let peers = context.node_table.read().nodes_with_services(&Services::default(), context.config.internet_protocol, &used_addresses, needed);
					let addresses = peers.into_iter()
						.map(|peer| peer.address())
						.collect::<Vec<_>>();

					info!("Creating {} more outbound connections", addresses.len());
					for address in addresses {
						Context::connect::<NormalSessionFactory>(context.clone(), address);
					}
				}

				if let Err(_err) = context.node_table.read().save_to_file(&context.config.node_table_path) {
					error!("Saving node table to disk failed");
				}

				Ok(())
			})
			.for_each(|_| Ok(()))
			.then(|_| finished(())));
		c.spawn(interval);
	}

	/// Connect to socket using given context and handle.
	fn connect_future<T>(context: Arc<Context>, socket: net::SocketAddr, handle: &Handle, config: &NetConfig) -> BoxedEmptyFuture where T: SessionFactory {
		info!("Trying to connect to: {}", socket);
		let connection = connect(&socket, handle, config);
		Box::new(connection.then(move |result| {
			match result {
				Ok(DeadlineStatus::Meet(Ok(connection))) => {
					// successfull hanshake
					info!("Connected to {}", connection.address);
					context.node_table.write().insert(connection.address, connection.services);
					//将连接好的connection,生成对应的channel  T 为SeednodeSessionFactory  所有创建的channel 支持的协议有PingProtocol，AddrProtocol，SeednodeProtocol
					let channel = context.connections.store::<T>(context.clone(), connection, Direction::Outbound);

					// initialize session and then start reading messages
					channel.session().initialize();
					Context::on_message(context, channel)
				},
				Ok(DeadlineStatus::Meet(Err(_))) => {
					// protocol error
					info!("Handshake with {} failed", socket);
					// TODO: close socket
					context.node_table.write().note_failure(&socket);
					context.connection_counter.note_close_outbound_connection();
					Box::new(finished(Ok(())))
				},
				Ok(DeadlineStatus::Timeout) => {
					// connection time out
					info!("Handshake with {} timed out", socket);
					// TODO: close socket
					context.node_table.write().note_failure(&socket);
					context.connection_counter.note_close_outbound_connection();
					Box::new(finished(Ok(())))
				},
				Err(_) => {
					// network error
					info!("Unable to connect to {}", socket);
					context.node_table.write().note_failure(&socket);
					context.connection_counter.note_close_outbound_connection();
					Box::new(finished(Ok(())))
				}
			}
		})
		.then(|_| finished(())))
	}

	/// Connect to socket using given context.
	///使用新的context 创建新的
	pub fn connect<T>(context: Arc<Context>, socket: net::SocketAddr) where T: SessionFactory {
		context.connection_counter.note_new_outbound_connection();
		context.remote.clone().spawn(move |handle| {
			let config = context.config.clone();
			//此处的T 为SeednodeSessionFactory
			context.pool.clone().spawn(Context::connect_future::<T>(context, socket, handle, &config.connection))
		})
	}

	pub fn connect_normal(context: Arc<Context>, socket: net::SocketAddr) {
		Self::connect::<NormalSessionFactory>(context, socket)
	}

	pub fn accept_connection_future(context: Arc<Context>, stream: TcpStream, socket: net::SocketAddr, handle: &Handle, config: NetConfig) -> BoxedEmptyFuture {
		//连接握手
		Box::new(accept_connection(stream, handle, &config, socket).then(move |result| {
			match result {
				Ok(DeadlineStatus::Meet(Ok(connection))) => {
					// successfull hanshake
					info!("Accepted connection from {}", connection.address);
					context.node_table.write().insert(connection.address, connection.services);
					let channel = context.connections.store::<NormalSessionFactory>(context.clone(), connection, Direction::Inbound);

					// initialize session and then start reading messages 
					//初始化里面，根据初始化的类型，1 需要发送一个getaddr的请求出去，在连接成功后需要获取对方的地址
					channel.session().initialize();
					Context::on_message(context.clone(), channel)
				},
				Ok(DeadlineStatus::Meet(Err(err))) => {
					// protocol error
					info!("Accepting handshake from {} failed with error: {}", socket, err);
					// TODO: close socket
					context.node_table.write().note_failure(&socket);
					context.connection_counter.note_close_inbound_connection();
					Box::new(finished(Ok(())))
				},
				Ok(DeadlineStatus::Timeout) => {
					// connection time out
					info!("Accepting handshake from {} timed out", socket);
					// TODO: close socket
					context.node_table.write().note_failure(&socket);
					context.connection_counter.note_close_inbound_connection();
					Box::new(finished(Ok(())))
				},
				Err(_) => {
					// network error
					info!("Accepting handshake from {} failed with network error", socket);
					context.node_table.write().note_failure(&socket);
					context.connection_counter.note_close_inbound_connection();
					Box::new(finished(Ok(())))
				}
			}
		})
		.then(|_| finished(())))
	}

	pub fn accept_connection(context: Arc<Context>, stream: TcpStream, socket: net::SocketAddr, config: NetConfig) {
		context.connection_counter.note_new_inbound_connection();
		//每接受一个节点的连接，就开启一个线程运行 难道就是这个原因？造成在运行pbtc的时候，会很卡
		context.remote.clone().spawn(move |handle| {
			context.pool.clone().spawn(Context::accept_connection_future(context, stream, socket, handle, config))
		})
	}

	/// Starts tcp server and listens for incomming connections. 这里使用的Context是节点自身
	pub fn listen(context: Arc<Context>, handle: &Handle, config: NetConfig) -> Result<BoxedEmptyFuture, io::Error> {
		info!("Starting tcp server");
		let server = try!(TcpListener::bind(&config.local_address, handle));
		let server = Box::new(server.incoming()
			.and_then(move |(stream, socket)| {
				// because we acquire atomic value twice,
				// it may happen that accept slightly more connections than we need
				// we don't mind 当接收到有新连接的时候，检查当前是否还能够继续accept 新连接
				if context.connection_counter.inbound_connections_needed() > 0 {
					//接受从另外的客户端来的请求
					Context::accept_connection(context.clone(), stream, socket, config.clone());
				} else {
					// ignore result
					let _ = stream.shutdown(net::Shutdown::Both);
				}
				Ok(())
			})
			.for_each(|_| Ok(()))
			.then(|_| finished(())));
		Ok(server)
	}

	/// Called on incomming mesage.
	pub fn on_message(context: Arc<Context>, channel: Arc<Channel>) -> IoFuture<MessageResult<()>> {
		Box::new(channel.read_message().then(move |result| {
			match result {
				Ok(Ok((command, payload))) => {
					// successful read
					info!("Received {} message from {}", command, channel.peer_info().address);
					// handle message and read the next one 根据消息，由不同的protocol来处理
					match channel.session().on_message(command, payload) {
						Ok(_) => {
							//在connection 接收到msg后，需要更新对应node的信息
							context.node_table.write().note_used(&channel.peer_info().address);
							//在更新节点之后，为什么还要继续调用on_message这个方法
							let on_message = Context::on_message(context.clone(), channel);
							context.spawn(on_message);
							Box::new(finished(Ok(())))
						},
						Err(err) => {
							// protocol error
							context.close_channel_with_error(channel.peer_info().id, &err);
							Box::new(finished(Err(err)))
						}
					}
				},
				Ok(Err(err)) => {
					// protocol error
					context.close_channel_with_error(channel.peer_info().id, &err);
					Box::new(finished(Err(err)))
				},
				Err(err) => {
					// network error
					// TODO: remote node was just turned off. should we mark it as not reliable?
					context.close_channel_with_error(channel.peer_info().id, &err);
					Box::new(failed(err))
				}
			}
		}))
	}

	/// Send message to a channel with given peer id.
	pub fn send_to_peer<T>(context: Arc<Context>, peer: PeerId, payload: &T, serialization_flags: u32) -> IoFuture<()> where T: Payload {
		match context.connections.channel(peer) {
			Some(channel) => {
				let info = channel.peer_info();
				let message = Message::with_flags(info.magic, info.version, payload, serialization_flags).expect("failed to create outgoing message");
				channel.session().stats().lock().report_send(T::command().into(), message.len());
				Context::send(context, channel, message)
			},
			None => {
				// peer no longer exists.
				// TODO: should we return error here?
				Box::new(finished(()))
			}
		}
	}

	pub fn send_message_to_peer<T>(context: Arc<Context>, peer: PeerId, message: T) -> IoFuture<()> where T: AsRef<[u8]> + Send + 'static {
		match context.connections.channel(peer) {
			Some(channel) => Context::send(context, channel, message),
			None => {
				// peer no longer exists.
				// TODO: should we return error here?
				Box::new(finished(()))
			}
		}
	}

	/// Send message using given channel.
	pub fn send<T>(_context: Arc<Context>, channel: Arc<Channel>, message: T) -> IoFuture<()> where T: AsRef<[u8]> + Send + 'static {
		//trace!("Sending {} message to {}", T::command(), channel.peer_info().address);
		//这个地方将msg通过channel 已经发送出去了
		Box::new(channel.write_message(message).then(move |result| {
			match result {
				Ok(_) => {
					// successful send
					//trace!("Sent {} message to {}", T::command(), channel.peer_info().address);
					Box::new(finished(()))
				},
				Err(err) => {
					// network error
					// closing connection is handled in on_message`
					Box::new(failed(err))
				},
			}
		}))
	}

	/// Close channel with given peer info.
	pub fn close_channel(&self, id: PeerId) {
		if let Some(channel) = self.connections.remove(id) {
			let info = channel.peer_info();
			channel.session().on_close();
			info!("Disconnecting from {}", info.address);
			channel.shutdown();
			match info.direction {
				Direction::Inbound => self.connection_counter.note_close_inbound_connection(),
				Direction::Outbound => self.connection_counter.note_close_outbound_connection(),
			}
		}
	}

	/// Close channel with given peer info.
	pub fn close_channel_with_error(&self, id: PeerId, error: &error::Error) {
		if let Some(channel) = self.connections.remove(id) {
			let info = channel.peer_info();
			channel.session().on_close();
			info!("Disconnecting from {} caused by {}", info.address, error.description());
			channel.shutdown();
			self.node_table.write().note_failure(&info.address);
			match info.direction {
				Direction::Inbound => self.connection_counter.note_close_inbound_connection(),
				Direction::Outbound => self.connection_counter.note_close_outbound_connection(),
			}
		}
	}

	pub fn create_sync_session(&self, start_height: i32, services: Services, outbound_connection: OutboundSyncConnectionRef) -> InboundSyncConnectionRef {
		self.local_sync_node.create_sync_session(start_height, services, outbound_connection)
	}

	pub fn connections(&self) -> &Connections {
		&self.connections
	}

	pub fn nodes(&self) -> Vec<Node> {
		self.node_table.read().nodes()
	}
}

pub struct P2P {
	/// Global event loop handle.
	event_loop_handle: Handle,
	/// Worker thread pool.
	pool: CpuPool,
	/// P2P config.
	config: Config,
	/// Network context.
	context: Arc<Context>,
}

impl Drop for P2P {
	fn drop(&mut self) {
		// there are retain cycles
		// context->connections->channel->session->protocol->context
		// context->connections->channel->on_message closure->context
		// first let's get rid of session retain cycle
		for channel in &self.context.connections.remove_all() {
			// done, now let's finish on_message
			channel.shutdown();
		}
	}
}

impl P2P {
	pub fn new(config: Config, local_sync_node: LocalSyncNodeRef, handle: Handle) -> Result<Self, Box<error::Error>> {
		let pool = CpuPool::new(config.threads);
		println!("config:{:?}",config);
		let context = try!(Context::new(local_sync_node, pool.clone(), handle.remote().clone(), config.clone()));

		let p2p = P2P {
			event_loop_handle: handle.clone(),
			pool: pool,
			context: Arc::new(context),
			config: config,
		};

		Ok(p2p)
	}

	pub fn run(&self) -> Result<(), Box<error::Error>> {
		
		//连接配置文件中的节点
		for peer in &self.config.peers {
			self.connect::<NormalSessionFactory>(*peer);
		}

		let resolver = try!(DnsResolver::system_config(&self.event_loop_handle));
		//根据种子文件域名 获取种子服务器对应的IP地址
		for seed in &self.config.seeds {
			println!("seed node info {}",seed);
			self.connect_to_seednode(&resolver, seed);
		}
		//自动连接,根据种子地址
		Context::autoconnect(self.context.clone(), &self.event_loop_handle);
		//开始监听，接受外来的会话连接
		try!(self.listen());
		Ok(())
	}

	/// Attempts to connect to the specified node
	pub fn connect<T>(&self, addr: net::SocketAddr) where T: SessionFactory {
		Context::connect::<T>(self.context.clone(), addr);
	}

	pub fn connect_to_seednode(&self, resolver: &Resolver, seednode: &str) {
		let owned_seednode = seednode.to_owned();

		let context = self.context.clone();
		let dns_lookup = resolver.resolve(seednode).then(move |result| {
	
			match result {
				//将指定的种子服务器域名进行解析成对应的IP地址，由于一个域名对应多个IP地址，这里是任意的选取了一个地址
				Ok(address) => match address.pick_one() {
					Some(socket) => {
						println!("Dns lookup of seednode {} finished. Connecting to {}", owned_seednode, socket);
						Context::connect::<SeednodeSessionFactory>(context, socket);
					},
					None => {
						info!("Dns lookup of seednode {} resolved with no results", owned_seednode);
					}
				},
				Err(_err) => {
					info!("Dns lookup of seednode {} failed", owned_seednode);
				}
			}
			finished(())
		});
		//执行一个闭包
		let pool_work = self.pool.spawn(dns_lookup);
		self.event_loop_handle.spawn(pool_work);
	}

	fn listen(&self) -> Result<(), Box<error::Error>> {
		let server = try!(Context::listen(self.context.clone(), &self.event_loop_handle, self.config.connection.clone()));
		self.event_loop_handle.spawn(server);
		Ok(())
	}

	pub fn context(&self) -> &Arc<Context> {
		&self.context
	}
}

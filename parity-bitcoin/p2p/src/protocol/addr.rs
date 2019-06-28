use std::sync::Arc;
use std::time::Duration;
use bytes::Bytes;
use message::{Error, Command, deserialize_payload, Payload};
use message::types::{GetAddr, Addr};
use protocol::Protocol;
use net::PeerContext;
use util::Direction;

pub struct AddrProtocol {
	/// Context
	context: Arc<PeerContext>,
	/// True if this is a connection to the seednode && we should disconnect after receiving addr message
	is_seed_node_connection: bool,
}

impl AddrProtocol {
	pub fn new(context: Arc<PeerContext>, is_seed_node_connection: bool) -> Self {
		AddrProtocol {
			context: context,
			is_seed_node_connection: is_seed_node_connection,
		}
	}
}

impl Protocol for AddrProtocol {
	fn initialize(&mut self) {
		if let Direction::Outbound = self.context.info().direction {
			self.context.send_request(&GetAddr);
		}
	}
	//这段代码是针对一个结点在不同的时候，扮演不同的身份，需要响应getaddr addr两种类型的消息
	fn on_message(&mut self, command: &Command, payload: &Bytes) -> Result<(), Error> {
		// normal nodes send addr message only after they receive getaddr message
		// meanwhile seednodes, surprisingly, send addr message even before they are asked for it
		// 通常情况下，节点收到关于地址相关的消息是在他们发送getaddr消息之后，与此同时，种子节点发送地址消息甚至在没有接收到地址请求之前
		if command == &GetAddr::command() {//getaddr

			println!("node receive addr command");
			let _: GetAddr = try!(deserialize_payload(payload, self.context.info().version));
			//响应最新的1000条地址回去
			let entries = self.context.global().node_table_entries().into_iter().map(Into::into).collect();
			let addr = Addr::new(entries);
			self.context.send_response_inline(&addr);
		} else if command == &Addr::command() {//在初始化阶段，节点收到getaddr 请求对应的响应，在这里进行处理
			//将负载数据 反序列化为 Addr类型数据
			let addr: Addr = try!(deserialize_payload(payload, self.context.info().version));
			println!("node get addr command");

			match addr {
				Addr::V0(_) => {
					unreachable!("This version of protocol is not supported!");
				},
				Addr::V31402(addr) => {
					let nodes_len = addr.addresses.len();
					println!("nodes len:{}",nodes_len);
					self.context.global().update_node_table(addr.addresses);
					// seednodes are currently responding with two addr messages:
					// 1) addr message with single address - seednode itself
					// 2) addr message with 1000 addresses (seednode node_table contents)
					//在请求种子节点时，会返回两种类型的消息，一种是种子节点自身，还有就是1000条种子节点记录的地址
					if self.is_seed_node_connection && nodes_len > 1 {
						self.context.close();
					}
				},
			}
		}
		Ok(())
	}
}

pub struct SeednodeProtocol {
	/// Context
	context: Arc<PeerContext>,
	/// Indicates if disconnecting has been scheduled.
	disconnecting: bool,
}

impl SeednodeProtocol {
	pub fn new(context: Arc<PeerContext>) -> Self {
		SeednodeProtocol {
			context: context,
			disconnecting: false,
		}
	}
}

impl Protocol for SeednodeProtocol {
	fn on_message(&mut self, command: &Command, _payload: &Bytes) -> Result<(), Error> {
		// Seednodes send addr message more than once with different addresses.
		// We can't disconenct after first read. Let's delay it by 60 seconds. 注册一个延时关闭channel的行为
		if !self.disconnecting && command == &Addr::command() {
			self.disconnecting = true;
			let context = self.context.global().clone();
			let peer = self.context.info().id;
			self.context.global().execute_after(Duration::new(60, 0), move || {
				context.close_channel(peer);
			});
		}
		Ok(())
	}
}

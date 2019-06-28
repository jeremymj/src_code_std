use std::sync::atomic::{AtomicUsize, Ordering};
use p2p::{LocalSyncNode, LocalSyncNodeRef, OutboundSyncConnectionRef, InboundSyncConnectionRef};
use message::Services;
use inbound_connection::InboundConnection;
use types::{PeersRef, LocalNodeRef};

/// Inbound synchronization connection factory
pub struct InboundConnectionFactory {
	/// Peers reference
	peers: PeersRef,
	/// Reference to synchronization node
	node: LocalNodeRef,
	/// Throughout counter of synchronization peers
	counter: AtomicUsize,
}

impl InboundConnectionFactory {
	/// Create new inbound connection factory
	pub fn new(peers: PeersRef, node: LocalNodeRef) -> Self {
		InboundConnectionFactory {
			peers: peers,
			node: node,
			counter: AtomicUsize::new(0),
		}
	}

	/// Box inbound connection factory
	pub fn boxed(self) -> LocalSyncNodeRef {
		Box::new(self)
	}
}

impl LocalSyncNode for InboundConnectionFactory {
	//创建链接同步会话
	fn create_sync_session(&self, _best_block_height: i32, services: Services, outbound_connection: OutboundSyncConnectionRef) -> InboundSyncConnectionRef {
		//fetch_add 添加一个目标值，返回更新前的值，使用这个标识 Ordering::SeqCst 可以让所有的线程都能够看到这个序列的变化，所以要得到当前的序列值，还需要加上 fetch_add 添加的值
		let peer_index = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
		trace!(target: "sync", "Creating new sync session with peer#{}", peer_index);
		// remember outbound connection
		self.peers.insert(peer_index, services, outbound_connection);
		// create new inbound connection
		InboundConnection::new(peer_index, self.peers.clone(), self.node.clone()).boxed()
	}
}

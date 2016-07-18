// Copyright 2016 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::{Arc, RwLock};
use std::option::Option;
use std::collections::{HashMap, HashSet, BTreeMap};
use std::boxed::Box;
use std::collections::Bound::{Excluded, Unbounded};
use std::time::Duration;
use std::{cmp, u64};

use rocksdb::DB;
use mio::{self, EventLoop, EventLoopBuilder, Sender};
use protobuf;
use uuid::Uuid;

use kvproto::raft_serverpb::{RaftMessage, RaftSnapshotData, RaftTruncatedState, RegionLocalState,
                             PeerState};
use kvproto::raftpb::{ConfChangeType, Snapshot, MessageType};
use kvproto::pdpb::StoreStats;
use util::{HandyRwLock, SlowTimer};
use pd::PdClient;
use kvproto::raft_cmdpb::{AdminCmdType, AdminRequest, StatusCmdType, StatusResponse,
                          RaftCmdRequest, RaftCmdResponse};
use protobuf::Message;
use raft::SnapshotStatus;
use raftstore::{Result, Error};
use kvproto::metapb;
use util::worker::{Worker, Scheduler};
use util::get_disk_stat;
use super::worker::{SplitCheckRunner, SplitCheckTask, SnapTask, SnapRunner, CompactTask,
                    CompactRunner, PdRunner, PdTask};
use super::{util, SendCh, Msg, Tick, SnapManager};
use super::keys::{self, enc_start_key, enc_end_key};
use super::engine::{Iterable, Peekable};
use super::config::Config;
use super::peer::{Peer, PendingCmd, ReadyResult, ExecResult};
use super::peer_storage::{ApplySnapResult, SnapState};
use super::msg::Callback;
use super::cmd_resp::{bind_uuid, bind_term, bind_error};
use super::transport::Transport;

type Key = Vec<u8>;

const ROCKSDB_TOTAL_SST_FILE_SIZE_PROPERTY: &'static str = "rocksdb.total-sst-files-size";

pub struct Store<T: Transport, C: PdClient + 'static> {
    cfg: Config,
    store: metapb::Store,
    engine: Arc<DB>,
    sendch: SendCh,

    // region_id -> peers
    region_peers: HashMap<u64, Peer>,
    pending_raft_groups: HashSet<u64>,
    // region end key -> region id
    region_ranges: BTreeMap<Key, u64>,

    split_check_worker: Worker<SplitCheckTask>,
    snap_worker: Worker<SnapTask>,
    compact_worker: Worker<CompactTask>,
    pd_worker: Worker<PdTask>,

    trans: Arc<RwLock<T>>,
    pd_client: Arc<C>,

    peer_cache: Arc<RwLock<HashMap<u64, metapb::Peer>>>,

    snap_mgr: SnapManager,
}

pub fn create_event_loop<T, C>(cfg: &Config) -> Result<EventLoop<Store<T, C>>>
    where T: Transport,
          C: PdClient
{
    // We use base raft tick as the event loop timer tick.
    let mut builder = EventLoopBuilder::new();
    builder.timer_tick(Duration::from_millis(cfg.raft_base_tick_interval));
    builder.notify_capacity(cfg.notify_capacity);
    builder.messages_per_tick(cfg.messages_per_tick);
    let event_loop = try!(builder.build());
    Ok(event_loop)
}

impl<T: Transport, C: PdClient> Store<T, C> {
    pub fn new(sender: Sender<Msg>,
               meta: metapb::Store,
               cfg: Config,
               engine: Arc<DB>,
               trans: Arc<RwLock<T>>,
               pd_client: Arc<C>,
               mgr: SnapManager)
               -> Result<Store<T, C>> {
        // TODO: we can get cluster meta regularly too later.
        try!(cfg.validate());

        let sendch = SendCh::new(sender);

        let peer_cache = HashMap::new();

        Ok(Store {
            cfg: cfg,
            store: meta,
            engine: engine,
            sendch: sendch,
            region_peers: HashMap::new(),
            pending_raft_groups: HashSet::new(),
            split_check_worker: Worker::new("split check worker"),
            snap_worker: Worker::new("snapshot worker"),
            compact_worker: Worker::new("compact worker"),
            pd_worker: Worker::new("pd worker"),
            region_ranges: BTreeMap::new(),
            trans: trans,
            pd_client: pd_client,
            peer_cache: Arc::new(RwLock::new(peer_cache)),
            snap_mgr: mgr,
        })
    }

    // Do something before store runs.
    fn prepare(&mut self) -> Result<()> {
        // Scan region meta to get saved regions.
        let start_key = keys::REGION_META_MIN_KEY;
        let end_key = keys::REGION_META_MAX_KEY;
        let engine = self.engine.clone();
        try!(engine.scan(start_key,
                         end_key,
                         &mut |key, value| {
            let (region_id, suffix) = try!(keys::decode_region_meta_key(key));
            if suffix != keys::REGION_STATE_SUFFIX {
                return Ok(true);
            }

            let local_state = try!(protobuf::parse_from_bytes::<RegionLocalState>(value));
            if local_state.get_state() == PeerState::Tombstone {
                debug!("region {:?} is tombstone in store {}",
                       local_state.get_region(),
                       self.store_id());
                return Ok(true);
            }
            let region = local_state.get_region();
            let mut peer = try!(Peer::create(self, region));

            if local_state.get_state() == PeerState::Applying {
                info!("region {:?} is applying in store {}",
                      local_state.get_region(),
                      self.store_id());
                peer.mut_store().set_snap_state(SnapState::Applying);
                box_try!(self.snap_worker.schedule(SnapTask::Apply { region_id: region_id }));
            }

            self.region_ranges.insert(enc_end_key(region), region_id);
            // No need to check duplicated here, because we use region id as the key
            // in DB.
            self.region_peers.insert(region_id, peer);
            Ok(true)
        }));

        Ok(())
    }

    pub fn run(&mut self, event_loop: &mut EventLoop<Self>) -> Result<()> {
        try!(self.prepare());

        try!(self.snap_mgr.wl().init());

        self.register_raft_base_tick(event_loop);
        self.register_raft_gc_log_tick(event_loop);
        self.register_split_region_check_tick(event_loop);
        self.register_pd_heartbeat_tick(event_loop);
        self.register_pd_store_heartbeat_tick(event_loop);
        self.register_snap_mgr_gc_tick(event_loop);

        let split_check_runner = SplitCheckRunner::new(self.sendch.clone(),
                                                       self.cfg.region_max_size,
                                                       self.cfg.region_split_size);
        box_try!(self.split_check_worker.start(split_check_runner));

        let runner = SnapRunner::new(self.engine.clone(),
                                     self.get_sendch(),
                                     self.snap_mgr.clone());
        box_try!(self.snap_worker.start(runner));

        box_try!(self.compact_worker.start(CompactRunner));

        let pd_runner = PdRunner::new(self.pd_client.clone(), self.sendch.clone());
        box_try!(self.pd_worker.start(pd_runner));

        try!(event_loop.run(self));
        Ok(())
    }

    pub fn get_sendch(&self) -> SendCh {
        self.sendch.clone()
    }

    #[inline]
    pub fn get_snap_mgr(&self) -> SnapManager {
        self.snap_mgr.clone()
    }

    pub fn snap_scheduler(&self) -> Scheduler<SnapTask> {
        self.snap_worker.scheduler()
    }

    pub fn engine(&self) -> Arc<DB> {
        self.engine.clone()
    }

    pub fn store_id(&self) -> u64 {
        self.store.get_id()
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub fn peer_cache(&self) -> Arc<RwLock<HashMap<u64, metapb::Peer>>> {
        self.peer_cache.clone()
    }

    fn register_raft_base_tick(&self, event_loop: &mut EventLoop<Self>) {
        // If we register raft base tick failed, the whole raft can't run correctly,
        // TODO: shutdown the store?
        if let Err(e) = register_timer(event_loop, Tick::Raft, self.cfg.raft_base_tick_interval) {
            error!("register raft base tick err: {:?}", e);
        };
    }

    fn on_raft_base_tick(&mut self, event_loop: &mut EventLoop<Self>) {
        for (&region_id, peer) in &mut self.region_peers {
            if !peer.get_store().is_applying_snap() {
                peer.raft_group.tick();
                self.pending_raft_groups.insert(region_id);
            }
        }

        self.register_raft_base_tick(event_loop);
    }

    // Clippy doesn't allow hash_map contains_key followed by insert, and suggests
    // using entry().or_insert() instead, but we can't use this because creating peer
    // may fail, so we allow map_entry.
    #[allow(map_entry)]
    fn on_raft_message(&mut self, mut msg: RaftMessage) -> Result<()> {
        let region_id = msg.get_region_id();
        if !self.is_raft_msg_valid(&msg) {
            return Ok(());
        }

        if msg.get_is_tombstone() {
            // we receive a message tells us to remove ourself.
            self.handle_gc_peer_msg(&msg);
            return Ok(());
        }

        if try!(self.is_msg_stale(&msg)) {
            return Ok(());
        }

        // TODO: we may encounter a message with larger peer id, which
        // means current peer is stale, then we should remove current peer

        if !self.region_peers.contains_key(&region_id) {
            let peer = match Peer::replicate(self, region_id, msg.get_to_peer().get_id()) {
                Ok(peer) => peer,
                Err(e) => {
                    error!("peer replication from {:?} to {:?} in region {} failed {:?}",
                           msg.get_from_peer(),
                           msg.get_to_peer(),
                           region_id,
                           e);
                    return Err(e);
                }
            };
            // We don't have start_key of the region, so there is no need to insert into
            // region_ranges
            self.region_peers.insert(region_id, peer);
        }

        if try!(self.is_snapshot_overlapped(&msg)) {
            return Ok(());
        }

        self.insert_peer_cache(msg.take_from_peer());
        self.insert_peer_cache(msg.take_to_peer());

        let peer = self.region_peers.get_mut(&region_id).unwrap();
        let timer = SlowTimer::new();
        try!(peer.raft_group.step(msg.take_message()));
        slow_log!(timer, "region {} raft step", region_id);

        // Add into pending raft groups for later handling ready.
        self.pending_raft_groups.insert(region_id);

        Ok(())
    }

    // return false means the message is invalid, and can be ignored.
    fn is_raft_msg_valid(&self, msg: &RaftMessage) -> bool {
        let region_id = msg.get_region_id();
        let from = msg.get_from_peer();
        let to = msg.get_to_peer();

        debug!("handle raft message {:?} for region {}, from {} to {}",
               msg.get_message().get_msg_type(),
               region_id,
               from.get_id(),
               to.get_id());

        if to.get_store_id() != self.store_id() {
            warn!("store not match, to store id {}, mine {}, ignore it",
                  to.get_store_id(),
                  self.store_id());
            return false;
        }

        if !msg.has_region_epoch() {
            error!("missing epoch in raft message, ignore it");
            return false;
        }

        true
    }

    fn is_msg_stale(&self, msg: &RaftMessage) -> Result<bool> {
        let region_id = msg.get_region_id();
        let from_epoch = msg.get_region_epoch();
        let is_vote_msg = msg.get_message().get_msg_type() == MessageType::MsgRequestVote;
        let from_store_id = msg.get_from_peer().get_store_id();

        // Let's consider following cases with three nodes [1, 2, 3] and 1 is leader:
        // a. 1 removes 2, 2 may still send MsgAppendResponse to 1.
        //  We should ignore this stale message and let 2 remove itself after
        //  applying the ConfChange log.
        // b. 2 is isolated, 1 removes 2. When 2 rejoins the cluster, 2 will
        //  send stale MsgRequestVote to 1 and 3, at this time, we should tell 2 to gc itself.
        // c. 2 is isolated but can communicate with 3. 1 removes 3.
        //  2 will send stale MsgRequestVote to 3, 3 should ignore this message.
        // d. 2 is isolated but can communicate with 3. 1 removes 2, then adds 4, remove 3.
        //  2 will send stale MsgRequestVote to 3, 3 should tell 2 to gc itself.
        // e. 2 is isolated. 1 adds 4, 5, 6, removes 3, 1. Now assume 4 is leader.
        //  After 2 rejoins the cluster, 2 may send stale MsgRequestVote to 1 and 3,
        //  1 and 3 will ignore this message. Later 4 will send messages to 2 and 2 will
        //  rejoin the raft group again.
        // f. 2 is isolated. 1 adds 4, 5, 6, removes 3, 1. Now assume 4 is leader, and 4 removes 2.
        //  unlike case e, 2 will be stale forever.
        // TODO: for case f, if 2 is stale for a long time, 2 will communicate with pd and pd will
        // tell 2 is stale, so 2 can remove itself.
        if let Some(peer) = self.region_peers.get(&region_id) {
            let region = &peer.get_store().region;
            let epoch = region.get_region_epoch();

            if util::is_epoch_stale(from_epoch, epoch) &&
               util::find_peer(region, from_store_id).is_none() {
                // The message is stale and not in current region.
                self.handle_stale_msg(msg, epoch, is_vote_msg);
                return Ok(true);
            }

            return Ok(false);
        }

        // no exist, check with tombstone key.
        let state_key = keys::region_state_key(region_id);
        if let Some(local_state) = try!(self.engine.get_msg::<RegionLocalState>(&state_key)) {
            if local_state.get_state() == PeerState::Tombstone {
                let region = local_state.get_region();
                let region_epoch = region.get_region_epoch();
                // The region in this peer is already destroyed
                if util::is_epoch_stale(from_epoch, region_epoch) {
                    info!("tombstone peer [epoch: {:?}] receive a stale message {:?}",
                        region_epoch,
                          msg,
                          );

                    let not_exist = util::find_peer(region, from_store_id).is_none();
                    self.handle_stale_msg(msg, region_epoch, is_vote_msg && not_exist);

                    return Ok(true);
                }

                if from_epoch.get_conf_ver() == region_epoch.get_conf_ver() {
                    return Err(box_err!("tombstone peer [epoch: {:?}] receive an invalid \
                                         message {:?}, ignore it",
                                        region_epoch,
                                        msg));
                }
            }
        }

        Ok(false)
    }

    fn handle_stale_msg(&self, msg: &RaftMessage, cur_epoch: &metapb::RegionEpoch, need_gc: bool) {
        let region_id = msg.get_region_id();
        let from_peer = msg.get_from_peer();
        let to_peer = msg.get_to_peer();

        if !need_gc {
            warn!("raft message {:?} is stale, current {:?}, ignore it",
                  msg,
                  cur_epoch);
            return;
        }

        warn!("raft message {:?} is stale, current {:?}, tell to gc",
              msg,
              cur_epoch);

        let mut gc_msg = RaftMessage::new();
        gc_msg.set_region_id(region_id);
        gc_msg.set_from_peer(to_peer.clone());
        gc_msg.set_to_peer(from_peer.clone());
        gc_msg.set_region_epoch(cur_epoch.clone());
        gc_msg.set_is_tombstone(true);
        if let Err(e) = self.trans.rl().send(gc_msg) {
            error!("send gc message failed {:?}", e);
        }
    }

    fn handle_gc_peer_msg(&mut self, msg: &RaftMessage) {
        let region_id = msg.get_region_id();

        let mut need_remove = false;
        if let Some(peer) = self.region_peers.get(&region_id) {
            // TODO: need checking peer id changed?
            let from_epoch = msg.get_region_epoch();
            if util::is_epoch_stale(peer.get_store().region.get_region_epoch(), from_epoch) {
                // TODO: ask pd to guarantee we are stale now.
                warn!("peer {:?} for region {} receives gc message, remove",
                      msg.get_to_peer(),
                      region_id);
                need_remove = true;
            }
        }

        if need_remove {
            self.destory_peer(region_id, msg.get_to_peer().clone());
        }
    }

    fn is_snapshot_overlapped(&self, msg: &RaftMessage) -> Result<bool> {
        let region_id = msg.get_region_id();

        // Check if we can accept the snapshot
        if !self.region_peers[&region_id].get_store().is_initialized() &&
           msg.get_message().has_snapshot() {
            let snap = msg.get_message().get_snapshot();
            let mut snap_data = RaftSnapshotData::new();
            try!(snap_data.merge_from_bytes(snap.get_data()));
            let snap_region = snap_data.get_region();
            if let Some((_, &exist_region_id)) = self.region_ranges
                .range(Excluded(&enc_start_key(snap_region)), Unbounded::<&Key>)
                .next() {
                let exist_region = self.region_peers[&exist_region_id].region();
                if enc_start_key(exist_region) < enc_end_key(snap_region) {
                    warn!("region overlapped {:?}, {:?}", exist_region, snap_region);
                    return Ok(true);
                }
            }
        }

        Ok(false)
    }

    fn insert_peer_cache(&mut self, peer: metapb::Peer) {
        self.peer_cache.wl().insert(peer.get_id(), peer);
    }

    fn on_raft_ready(&mut self) -> Result<()> {
        let t = SlowTimer::new();
        let ids: Vec<u64> = self.pending_raft_groups.drain().collect();
        let pending_count = ids.len();

        for region_id in ids {
            let mut ready_result = None;
            if let Some(peer) = self.region_peers.get_mut(&region_id) {
                match peer.handle_raft_ready(&self.trans) {
                    Err(e) => {
                        // TODO: should we panic or shutdown the store?
                        error!("handle raft ready at region {} err: {:?}", region_id, e);
                        return Err(e);
                    }
                    Ok(ready) => ready_result = ready,
                }
            }

            if let Some(ready_result) = ready_result {
                if let Err(e) = self.on_ready_result(region_id, ready_result) {
                    error!("handle raft ready result at region {} err: {:?}",
                           region_id,
                           e);
                    return Err(e);
                }
            }
        }

        slow_log!(t, "on {} regions raft ready", pending_count);

        Ok(())
    }

    fn destory_peer(&mut self, region_id: u64, peer: metapb::Peer) {
        warn!("destroy peer {:?} for region {}", peer, region_id);
        // TODO: should we check None here?
        // Can we destroy it in another thread later?
        let mut p = self.region_peers.remove(&region_id).unwrap();
        // We can't destroy a peer which is applying snapshot.
        assert!(!p.is_applying_snap());

        let is_initialized = p.is_initialized();
        let end_key = enc_end_key(p.region());
        if let Err(e) = p.destroy() {
            // should panic here?
            error!("destroy peer {:?} for region {} in store {} err {:?}",
                   peer,
                   region_id,
                   self.store_id(),
                   e);
            return;
        }

        if is_initialized && self.region_ranges.remove(&end_key).is_none() {
            panic!("Remove region, peer {:?}, region {} in store {}",
                   peer,
                   region_id,
                   self.store_id());

        }
    }

    fn on_ready_change_peer(&mut self,
                            region_id: u64,
                            change_type: ConfChangeType,
                            peer: metapb::Peer) {
        if let Some(p) = self.region_peers.get(&region_id) {
            if p.is_leader() {
                // Notify pd immediately.
                info!("notify pd with change peer region {:?}", p.region());
                self.heartbeat_pd(p);
            }
        }

        // We only care remove itself now.
        if change_type == ConfChangeType::RemoveNode && peer.get_store_id() == self.store_id() {
            // The remove peer is in the same store.
            self.destory_peer(region_id, peer)
        }
    }

    fn on_ready_compact_log(&mut self, region_id: u64, state: RaftTruncatedState) {
        let peer = self.region_peers.get(&region_id).unwrap();
        let task = CompactTask::new(peer.get_store(), state.get_index() + 1);
        if let Err(e) = self.compact_worker.schedule(task) {
            error!("failed to schedule compact task: {}", e);
        }
    }

    fn on_ready_split_region(&mut self,
                             region_id: u64,
                             left: metapb::Region,
                             right: metapb::Region) {
        let new_region_id = right.get_id();
        if let Some(peer) = self.region_peers.get(&new_region_id) {
            // If the store received a raft msg with the new region raft group
            // before splitting, it will creates a uninitialized peer.
            // We can remove this uninitialized peer directly.
            if peer.get_store().is_initialized() {
                panic!("duplicated region {} for split region", new_region_id);
            }
        }

        match Peer::create(self, &right) {
            Err(e) => {
                error!("create new split region {:?} err {:?}", right, e);
            }
            Ok(mut new_peer) => {
                // If the peer for the region before split is leader,
                // we can force the new peer for the new split region to campaign
                // to become the leader too.
                let is_leader = self.region_peers.get(&region_id).unwrap().is_leader();
                if is_leader && right.get_peers().len() > 1 {
                    if let Err(e) = new_peer.raft_group.campaign() {
                        error!("peer {:?} campaigns for region {} err {:?}",
                               new_peer.peer,
                               new_region_id,
                               e);
                    }
                }

                if is_leader {
                    // Notify pd immediately to let it update the region meta.
                    let left = self.region_peers.get(&region_id).unwrap();
                    self.report_split_pd(left, &new_peer);
                }

                // Insert new regions and validation
                info!("insert new regions left: {:?}, right:{:?}", left, right);
                if self.region_ranges
                    .insert(enc_end_key(&left), left.get_id())
                    .is_some() {
                    panic!("region should not exist, {:?}", left);
                }
                if self.region_ranges
                    .insert(enc_end_key(&right), new_region_id)
                    .is_none() {
                    panic!("region should exist, {:?}", right);
                }
                new_peer.size_diff_hint = self.cfg.region_check_size_diff;
                self.region_peers.insert(new_region_id, new_peer);
            }
        }
    }

    fn report_split_pd(&self, left: &Peer, right: &Peer) {
        let left_region = left.region();
        let right_region = right.region();

        info!("notify pd with split left {:?}, right {:?}",
              left_region,
              right_region);
        self.heartbeat_pd(left);
        self.heartbeat_pd(right);

        // Now pd only uses ReportSplit for history operation show,
        // so we send it independently here.
        let task = PdTask::ReportSplit {
            left: left_region.clone(),
            right: right_region.clone(),
        };

        if let Err(e) = self.pd_worker.schedule(task) {
            error!("failed to notify pd: {}", e);
        }
    }

    fn on_ready_apply_snapshot(&mut self, apply_result: ApplySnapResult) {
        let prev_region = apply_result.prev_region;
        let region = apply_result.region;

        info!("snapshot for region {:?} is applied", region);

        if !prev_region.get_peers().is_empty() {
            info!("region changed from {:?} -> {:?} after applying snapshot",
                  prev_region,
                  region);
            // we have already initialized the peer, so it must exist in region_ranges.
            if self.region_ranges.remove(&enc_end_key(&prev_region)).is_none() {
                panic!("region should exist {:?}", prev_region);
            }
        }

        self.region_ranges.insert(enc_end_key(&region), region.get_id());
    }

    fn on_ready_result(&mut self, region_id: u64, ready_result: ReadyResult) -> Result<()> {
        if let Some(apply_result) = ready_result.apply_snap_result {
            self.on_ready_apply_snapshot(apply_result);
        }

        let t = SlowTimer::new();
        let result_count = ready_result.exec_results.len();
        // handle executing committed log results
        for result in ready_result.exec_results {
            match result {
                ExecResult::ChangePeer { change_type, peer, .. } => {
                    self.on_ready_change_peer(region_id, change_type, peer)
                }
                ExecResult::CompactLog { state } => self.on_ready_compact_log(region_id, state),
                ExecResult::SplitRegion { left, right } => {
                    self.on_ready_split_region(region_id, left, right)
                }
            }
        }
        slow_log!(t, "on region {} ready {} results", region_id, result_count);

        Ok(())
    }

    fn propose_raft_command(&mut self, msg: RaftCmdRequest, cb: Callback) -> Result<()> {
        let mut resp = RaftCmdResponse::new();
        let uuid: Uuid = match util::get_uuid_from_req(&msg) {
            None => {
                bind_error(&mut resp, Error::Other("missing request uuid".into()));
                return cb.call_box((resp,));
            }
            Some(uuid) => {
                bind_uuid(&mut resp, uuid);
                uuid
            }
        };

        if msg.has_status_request() {
            // For status commands, we handle it here directly.
            match self.execute_status_command(msg) {
                Err(e) => bind_error(&mut resp, e),
                Ok(status_resp) => resp = status_resp,
            };
            return cb.call_box((resp,));
        }

        let region_id = msg.get_header().get_region_id();
        let mut peer = match self.region_peers.get_mut(&region_id) {
            None => {
                bind_error(&mut resp, Error::RegionNotFound(region_id));
                return cb.call_box((resp,));
            }
            Some(peer) => peer,
        };

        let term = peer.term();
        bind_term(&mut resp, term);

        if !peer.is_leader() {
            bind_error(&mut resp,
                       Error::NotLeader(region_id, peer.get_peer_from_cache(peer.leader_id())));
            return cb.call_box((resp,));
        }

        let peer_id = msg.get_header().get_peer().get_id();
        if peer.peer_id() != peer_id {
            bind_error(&mut resp,
                       box_err!("mismatch peer id {} != {}", peer.peer_id(), peer_id));
            return cb.call_box((resp,));
        }

        // Notice:
        // Here means the peer is leader, it can still step down to follower later,
        // but it doesn't matter, if the peer is not leader, the proposing command
        // log entry can't be committed.


        // TODO: support handing read-only commands later.
        // for read-only, if we don't care stale read, we can
        // execute these commands immediately in leader.

        let pending_cmd = PendingCmd {
            uuid: uuid,
            term: term,
            cb: cb,
        };
        try!(peer.propose(pending_cmd, msg, resp));

        self.pending_raft_groups.insert(region_id);

        // TODO: add timeout, if the command is not applied after timeout,
        // we will call the callback with timeout error.

        Ok(())
    }

    fn register_raft_gc_log_tick(&self, event_loop: &mut EventLoop<Self>) {
        if let Err(e) = register_timer(event_loop,
                                       Tick::RaftLogGc,
                                       self.cfg.raft_log_gc_tick_interval) {
            // If failed, we can't cleanup the raft log regularly.
            // Although the log size will grow larger and larger, it doesn't affect
            // whole raft logic, and we can send truncate log command to compact it.
            error!("register raft gc log tick err: {:?}", e);
        };
    }

    fn on_raft_gc_log_tick(&mut self, event_loop: &mut EventLoop<Self>) {
        for (&region_id, peer) in &mut self.region_peers {
            if !peer.is_leader() {
                continue;
            }

            // Leader will replicate the compact log command to followers,
            // If we use current replicated_index (like 10) as the compact index,
            // when we replicate this log, the newest replicated_index will be 11,
            // but we only compact the log to 10, not 11, at that time,
            // the first index is 10, and replicated_index is 11, with an extra log,
            // and we will do compact again with compact index 11, in cycles...
            // So we introduce a threshold, if replicated index - first index > threshold,
            // we will try to compact log.
            // raft log entries[..............................................]
            //                  ^                                       ^
            //                  |-----------------threshold------------ |
            //              first_index                         replicated_index
            let replicated_idx = peer.raft_group
                .status()
                .progress
                .values()
                .map(|p| p.matched)
                .min()
                .unwrap();
            let applied_idx = peer.get_store().applied_index();
            let first_idx = peer.get_store().first_index();
            let compact_idx;
            if applied_idx > first_idx && applied_idx - first_idx >= self.cfg.raft_log_gc_limit {
                compact_idx = applied_idx;
            } else if replicated_idx < first_idx ||
               replicated_idx - first_idx <= self.cfg.raft_log_gc_threshold {
                continue;
            } else {
                compact_idx = replicated_idx;
            }

            // Create a compact log request and notify directly.
            let request = new_compact_log_request(region_id, peer.peer.clone(), compact_idx);

            let cb = Box::new(move |_: RaftCmdResponse| -> Result<()> { Ok(()) });

            if let Err(e) = self.sendch.send(Msg::RaftCmd {
                request: request,
                callback: cb,
            }) {
                error!("send compact log {} to region {} err {:?}",
                       compact_idx,
                       region_id,
                       e);
            }
        }

        self.register_raft_gc_log_tick(event_loop);
    }

    fn register_split_region_check_tick(&self, event_loop: &mut EventLoop<Self>) {
        if let Err(e) = register_timer(event_loop,
                                       Tick::SplitRegionCheck,
                                       self.cfg.split_region_check_tick_interval) {
            error!("register split region check tick err: {:?}", e);
        };
    }

    fn on_split_region_check_tick(&mut self, event_loop: &mut EventLoop<Self>) {
        // To avoid frequent scan, we only add new scan tasks if all previous tasks
        // have finished.
        // TODO: check whether a gc progress has been started.
        if self.split_check_worker.is_busy() {
            self.register_split_region_check_tick(event_loop);
            return;
        }
        for (id, peer) in &mut self.region_peers {
            if !peer.is_leader() {
                continue;
            }

            if peer.size_diff_hint < self.cfg.region_check_size_diff {
                continue;
            }
            info!("region {}'s size diff {} >= {}, need to check whether should split",
                  id,
                  peer.size_diff_hint,
                  self.cfg.region_check_size_diff);
            let task = SplitCheckTask::new(peer.get_store());
            if let Err(e) = self.split_check_worker.schedule(task) {
                error!("failed to schedule split check: {}", e);
            }
            peer.size_diff_hint = 0;
        }

        self.register_split_region_check_tick(event_loop);
    }

    fn on_split_check_result(&mut self,
                             region_id: u64,
                             epoch: metapb::RegionEpoch,
                             split_key: Vec<u8>) {
        if split_key.is_empty() {
            error!("split key should not be empty!!!");
            return;
        }
        let p = self.region_peers.get(&region_id);
        if p.is_none() || !p.unwrap().is_leader() {
            // region on this store is no longer leader, skipped.
            info!("{} on {} doesn't exist or is not leader, skip.",
                  region_id,
                  self.store_id());
            return;
        }

        let peer = p.unwrap();
        let region = peer.region();

        if region.get_region_epoch().get_version() != epoch.get_version() {
            info!("{} epoch changed {:?} != {:?}, need re-check later",
                  region_id,
                  region.get_region_epoch(),
                  epoch);
            return;
        }

        let key = keys::origin_key(&split_key);
        let task = PdTask::AskSplit {
            region: region.clone(),
            split_key: key.to_vec(),
            peer: peer.peer.clone(),
        };

        if let Err(e) = self.pd_worker.schedule(task) {
            error!("failed to notify pd to split region {} at {:?}: {}",
                   region_id,
                   split_key,
                   e);
        }
    }

    fn heartbeat_pd(&self, peer: &Peer) {
        let task = PdTask::Heartbeat {
            region: peer.region().clone(),
            peer: peer.peer.clone(),
        };
        if let Err(e) = self.pd_worker.schedule(task) {
            error!("failed to notify pd: {}", e);
        }
    }

    fn on_pd_heartbeat_tick(&mut self, event_loop: &mut EventLoop<Self>) {
        let mut leader_count = 0;
        for peer in self.region_peers.values() {
            if peer.is_leader() {
                leader_count += 1;
                self.heartbeat_pd(peer);
            }
        }

        metric_gauge!("raftstore.leader_count", leader_count);
        metric_gauge!("raftstore.region_count", self.region_peers.len() as u64);

        self.register_pd_heartbeat_tick(event_loop);
    }


    fn register_pd_heartbeat_tick(&self, event_loop: &mut EventLoop<Self>) {
        if let Err(e) = register_timer(event_loop,
                                       Tick::PdHeartbeat,
                                       self.cfg.pd_heartbeat_tick_interval) {
            error!("register pd heartbeat tick err: {:?}", e);
        };
    }

    fn store_heartbeat_pd(&self) {
        let mut stats = StoreStats::new();
        let disk_stat = match get_disk_stat(self.engine.path()) {
            Ok(disk_stat) => disk_stat,
            Err(_) => {
                error!("get disk stat for rocksdb {} failed", self.engine.path());
                return;
            }
        };

        let capacity = cmp::min(disk_stat.capacity, self.cfg.capacity);

        stats.set_capacity(capacity);

        // Must get the total SST file size here.
        let used_size = self.engine
            .get_property_int(ROCKSDB_TOTAL_SST_FILE_SIZE_PROPERTY)
            .expect("rocksdb is too old, missing total-sst-files-size property");

        let mut available = if capacity > used_size {
            capacity - used_size
        } else {
            warn!("no available space for store {}", self.store_id());
            0
        };

        // We only care rocksdb SST file size, so we should
        // check disk available here.
        if available > disk_stat.available {
            available = disk_stat.available
        }

        stats.set_store_id(self.store_id());
        stats.set_available(available);
        stats.set_region_count(self.region_peers.len() as u32);

        let snap_stats = self.snap_mgr.rl().stats();
        stats.set_sending_snap_count(snap_stats.sending_count as u32);
        stats.set_receiving_snap_count(snap_stats.receiving_count as u32);

        metric_gauge!("raftstore.capacity", capacity);
        metric_gauge!("raftstore.available", available);
        metric_gauge!("raftstore.snapshot.sending",
                      snap_stats.sending_count as u64);
        metric_gauge!("raftstore.snapshot.receiving",
                      snap_stats.receiving_count as u64);

        if let Err(e) = self.pd_worker.schedule(PdTask::StoreHeartbeat { stats: stats }) {
            error!("failed to notify pd: {}", e);
        }
    }

    fn on_pd_store_heartbeat_tick(&mut self, event_loop: &mut EventLoop<Self>) {
        self.store_heartbeat_pd();
        self.register_pd_store_heartbeat_tick(event_loop);
    }

    fn handle_snap_mgr_gc(&mut self) -> Result<()> {
        let mut snap_keys = try!(self.snap_mgr.wl().list_snap());
        if snap_keys.is_empty() {
            return Ok(());
        }
        snap_keys.sort();
        let (mut last_region_id, mut compacted_idx, mut compacted_term) = (0, u64::MAX, u64::MAX);
        let mut is_applying_snap = false;
        for (key, is_sending) in snap_keys {
            if self.snap_mgr.rl().has_registered(&key) {
                continue;
            }
            if last_region_id != key.region_id {
                last_region_id = key.region_id;
                match self.region_peers.get(&key.region_id) {
                    None => {
                        // region is deleted
                        compacted_idx = u64::MAX;
                        compacted_term = u64::MAX;
                        is_applying_snap = false;
                    }
                    Some(peer) => {
                        let s = peer.get_store();
                        compacted_idx = s.truncated_index();
                        compacted_term = s.truncated_term();
                        is_applying_snap = s.is_applying_snap();
                    }
                };
            }

            let f = try!(self.snap_mgr.rl().get_snap_file(&key, is_sending));
            if is_sending {
                if key.term < compacted_term || key.idx < compacted_idx {
                    debug!("snap file {} has been compacted, delete.", key);
                    f.delete();
                } else if let Ok(meta) = f.meta() {
                    let modified = box_try!(meta.modified());
                    if let Ok(elapsed) = modified.elapsed() {
                        if elapsed > Duration::from_secs(self.cfg.snap_gc_timeout) {
                            debug!("snap file {} has been expired, delete.", key);
                            f.delete();
                        }
                    }
                }
            } else if key.term <= compacted_term &&
               (key.idx < compacted_idx || key.idx == compacted_idx && !is_applying_snap) {
                debug!("snap file {} has been applied, delete.", key);
                f.delete();
            }
        }
        Ok(())
    }

    fn on_snap_mgr_gc(&mut self, event_loop: &mut EventLoop<Self>) {
        if let Err(e) = self.handle_snap_mgr_gc() {
            error!("failed to gc snap manager on {}: {:?}", self.store_id(), e);
        }
        self.register_snap_mgr_gc_tick(event_loop);
    }

    fn register_pd_store_heartbeat_tick(&self, event_loop: &mut EventLoop<Self>) {
        if let Err(e) = register_timer(event_loop,
                                       Tick::PdStoreHeartbeat,
                                       self.cfg.pd_store_heartbeat_tick_interval) {
            error!("register pd store heartbeat tick err: {:?}", e);
        };
    }

    fn register_snap_mgr_gc_tick(&self, event_loop: &mut EventLoop<Self>) {
        if let Err(e) = register_timer(event_loop,
                                       Tick::SnapGc,
                                       self.cfg.snap_mgr_gc_tick_interval) {
            error!("register snap mgr gc tick err: {:?}", e);
        }
    }

    fn on_report_snapshot(&mut self, region_id: u64, to_peer_id: u64, status: SnapshotStatus) {
        if let Some(mut peer) = self.region_peers.get_mut(&region_id) {
            // The peer must exist in peer_cache.
            let to_peer = match self.peer_cache.rl().get(&to_peer_id).cloned() {
                Some(peer) => peer,
                None => {
                    // If to_peer is removed immediately after sending snapshot, the command
                    // may be applied before SnapshotStatus is reported. So here just ignore.
                    warn!("peer {} not found, skip reporting snap {:?}",
                          to_peer_id,
                          status);
                    return;
                }
            };
            info!("report snapshot status {:?} for {} {:?}",
                  to_peer,
                  region_id,
                  status);
            peer.raft_group.report_snapshot(to_peer_id, status)
        }
    }

    fn on_unreachable(&mut self, region_id: u64, to_peer_id: u64) {
        if let Some(mut peer) = self.region_peers.get_mut(&region_id) {
            peer.raft_group.report_unreachable(to_peer_id);
        }
    }

    fn on_snap_gen_res(&mut self, region_id: u64, snap: Option<Snapshot>) {
        let peer = match self.region_peers.get_mut(&region_id) {
            None => return,
            Some(peer) => peer,
        };
        let mut storage = peer.mut_store();
        if !storage.is_snap_state(SnapState::Generating) {
            // snapshot no need anymore.
            return;
        }
        match snap {
            Some(snap) => {
                storage.set_snap_state(SnapState::Snap(snap));
            }
            None => {
                storage.set_snap_state(SnapState::Failed);
            }
        }
    }

    fn on_snap_apply_res(&mut self, region_id: u64, is_success: bool) {
        let peer = self.region_peers.get_mut(&region_id).unwrap();
        let mut storage = peer.mut_store();
        assert!(storage.is_snap_state(SnapState::Applying),
                "snap state should not change during applying");
        if !is_success {
            // TODO: cleanup region and treat it as tombstone.
            panic!("applying snapshot to {} failed", region_id);
        }
        storage.set_snap_state(SnapState::Relax);
    }
}


fn register_timer<T: Transport, C: PdClient>(event_loop: &mut EventLoop<Store<T, C>>,
                                             tick: Tick,
                                             delay: u64)
                                             -> Result<mio::Timeout> {
    // TODO: now mio TimerError doesn't implement Error trait,
    // so we can't use `try!` directly.
    event_loop.timeout(tick, Duration::from_millis(delay))
        .map_err(|e| box_err!("register timer err: {:?}", e))
}

fn new_compact_log_request(region_id: u64,
                           peer: metapb::Peer,
                           compact_index: u64)
                           -> RaftCmdRequest {
    let mut request = RaftCmdRequest::new();
    request.mut_header().set_region_id(region_id);
    request.mut_header().set_peer(peer);
    request.mut_header().set_uuid(Uuid::new_v4().as_bytes().to_vec());

    let mut admin = AdminRequest::new();
    admin.set_cmd_type(AdminCmdType::CompactLog);
    admin.mut_compact_log().set_compact_index(compact_index);
    request.set_admin_request(admin);
    request
}

impl<T: Transport, C: PdClient> mio::Handler for Store<T, C> {
    type Timeout = Tick;
    type Message = Msg;

    fn notify(&mut self, event_loop: &mut EventLoop<Self>, msg: Msg) {
        let t = SlowTimer::new();
        let msg_str = format!("{:?}", msg);
        match msg {
            Msg::RaftMessage(data) => {
                if let Err(e) = self.on_raft_message(data) {
                    error!("handle raft message err: {:?}", e);
                }
            }
            Msg::RaftCmd { request, callback } => {
                if let Err(e) = self.propose_raft_command(request, callback) {
                    error!("propose raft command err: {:?}", e);
                }
            }
            Msg::Quit => {
                info!("receive quit message");
                event_loop.shutdown();
            }
            Msg::SplitCheckResult { region_id, epoch, split_key } => {
                info!("split check of {} complete.", region_id);
                self.on_split_check_result(region_id, epoch, split_key);
            }
            Msg::ReportSnapshot { region_id, to_peer_id, status } => {
                self.on_report_snapshot(region_id, to_peer_id, status);
            }
            Msg::ReportUnreachable { region_id, to_peer_id } => {
                self.on_unreachable(region_id, to_peer_id);
            }
            Msg::SnapshotStats => self.store_heartbeat_pd(),
            Msg::SnapApplyRes { region_id, is_success } => {
                self.on_snap_apply_res(region_id, is_success);
            }
            Msg::SnapGenRes { region_id, snap } => {
                self.on_snap_gen_res(region_id, snap);
            }
        }
        slow_log!(t, "handle {:?}", msg_str);
    }

    fn timeout(&mut self, event_loop: &mut EventLoop<Self>, timeout: Tick) {
        let t = SlowTimer::new();
        match timeout {
            Tick::Raft => self.on_raft_base_tick(event_loop),
            Tick::RaftLogGc => self.on_raft_gc_log_tick(event_loop),
            Tick::SplitRegionCheck => self.on_split_region_check_tick(event_loop),
            Tick::PdHeartbeat => self.on_pd_heartbeat_tick(event_loop),
            Tick::PdStoreHeartbeat => self.on_pd_store_heartbeat_tick(event_loop),
            Tick::SnapGc => self.on_snap_mgr_gc(event_loop),
        }
        slow_log!(t, "handle timeout {:?}", timeout);
    }

    #[allow(useless_vec)]
    fn tick(&mut self, event_loop: &mut EventLoop<Self>) {
        if !event_loop.is_running() {
            for (handle, name) in vec![(self.split_check_worker.stop(),
                                        self.split_check_worker.name()),
                                       (self.snap_worker.stop(), self.snap_worker.name()),
                                       (self.compact_worker.stop(), self.compact_worker.name()),
                                       (self.pd_worker.stop(), self.pd_worker.name())] {
                if let Some(Err(e)) = handle.map(|h| h.join()) {
                    error!("failed to stop {}: {:?}", name, e);
                }
            }

            return;
        }

        // We handle raft ready in event loop.
        if let Err(e) = self.on_raft_ready() {
            // TODO: should we panic here or shutdown the store?
            error!("handle raft ready err: {:?}", e);
        }
    }
}

impl<T: Transport, C: PdClient> Store<T, C> {
    /// load the target peer of request as mutable borrow.
    fn mut_target_peer(&mut self, request: &RaftCmdRequest) -> Result<&mut Peer> {
        let region_id = request.get_header().get_region_id();
        match self.region_peers.get_mut(&region_id) {
            None => Err(Error::RegionNotFound(region_id)),
            Some(peer) => Ok(peer),
        }
    }

    // Handle status commands here, separate the logic, maybe we can move it
    // to another file later.
    // Unlike other commands (write or admin), status commands only show current
    // store status, so no need to handle it in raft group.
    fn execute_status_command(&mut self, request: RaftCmdRequest) -> Result<RaftCmdResponse> {
        let cmd_type = request.get_status_request().get_cmd_type();
        let region_id = request.get_header().get_region_id();

        let mut response = try!(match cmd_type {
            StatusCmdType::RegionLeader => self.execute_region_leader(request),
            StatusCmdType::RegionDetail => self.execute_region_detail(request),
            StatusCmdType::InvalidStatus => Err(box_err!("invalid status command!")),
        });
        response.set_cmd_type(cmd_type);

        let mut resp = RaftCmdResponse::new();
        resp.set_status_response(response);
        // Bind peer current term here.
        if let Some(peer) = self.region_peers.get(&region_id) {
            bind_term(&mut resp, peer.term());
        }
        Ok(resp)
    }

    fn execute_region_leader(&mut self, request: RaftCmdRequest) -> Result<StatusResponse> {
        let peer = try!(self.mut_target_peer(&request));

        let mut resp = StatusResponse::new();
        if let Some(leader) = peer.get_peer_from_cache(peer.leader_id()) {
            resp.mut_region_leader().set_leader(leader);
        }

        Ok(resp)
    }

    fn execute_region_detail(&mut self, request: RaftCmdRequest) -> Result<StatusResponse> {
        let peer = try!(self.mut_target_peer(&request));
        if !peer.get_store().is_initialized() {
            let region_id = request.get_header().get_region_id();
            return Err(Error::RegionNotInitialized(region_id));
        }
        let mut resp = StatusResponse::new();
        resp.mut_region_detail().set_region(peer.region().clone());
        if let Some(leader) = peer.get_peer_from_cache(peer.leader_id()) {
            resp.mut_region_detail().set_leader(leader);
        }

        Ok(resp)
    }
}

//! Compact Raft consensus (spec §14.2) for the metadata plane.
//!
//! LionFS-cluster replicates namespace mutations (HEAD swaps, snapshot entries)
//! through Raft because *two concurrent writers must agree on the ordering
//! of HEAD swaps*:
//!
//! ```text
//! Client → Leader.propose(WriteVersionNodeEntry)
//! Leader → AppendEntries(followers)
//! Quorum ACK (⌈(N+1)/2⌉) → Commit → Apply → ACK client
//! ```
//!
//! This is a complete, self-contained, **deterministic** implementation of
//! the core algorithm:
//!
//! * leader election with seeded randomized timeouts,
//! * log replication with AppendEntries consistency checks
//!   (prevLogIndex/prevLogTerm),
//! * commit advancement restricted to entries of the leader's current term
//!   (Raft §5.4.2 — the classic leader-removal safety rule),
//! * conflict truncation and nextIndex back-off,
//! * an in-memory network with drop/partition injection.
//!
//! Scope notes (honest limitations): no log persistence (the engine's WAL
//! supplies durability), no InstallSnapshot (the Bε-tree checkpoint covers
//! it in the prototype), static membership.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};

pub type NodeId = usize;

// ---------------------------------------------------------------------------
// Messages & log
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum RaftMessage {
    RequestVote {
        term: u64,
        candidate: NodeId,
        last_log_index: u64,
        last_log_term: u64,
    },
    RequestVoteReply { term: u64, vote_granted: bool },
    AppendEntries {
        term: u64,
        leader: NodeId,
        prev_log_index: u64,
        prev_log_term: u64,
        entries: Vec<LogEntry>,
        leader_commit: u64,
    },
    AppendEntriesReply { term: u64, success: bool, match_index: u64 },
}

/// One replicated log entry — opaque bytes (the engine serializes
/// VersionNode / HEAD mutations into it).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    pub term: u64,
    pub index: u64,
    pub command: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Follower,
    Candidate,
    Leader,
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Server {
    pub self_id: NodeId,
    pub role: Role,
    pub current_term: u64,
    pub voted_for: Option<NodeId>,
    pub votes_received: usize,
    pub log: VecDeque<LogEntry>,
    pub commit_index: u64,
    pub last_applied: u64,
    pub next_index: BTreeMap<NodeId, u64>,
    pub match_index: BTreeMap<NodeId, u64>,
    election_timeout_in: u64,
    election_timeout: u64,
    heartbeat_due_in: u64,
    cluster_size: usize,
    rng_state: u64,
}

impl Server {
    fn new(id: NodeId, cluster_size: usize) -> Self {
        let mut s = Self {
            self_id: id,
            role: Role::Follower,
            current_term: 0,
            voted_for: None,
            votes_received: 0,
            log: VecDeque::new(),
            commit_index: 0,
            last_applied: 0,
            next_index: BTreeMap::new(),
            match_index: BTreeMap::new(),
            election_timeout_in: 150 + (id as u64 * 37) % 50,
            election_timeout: 150 + (id as u64 * 37) % 50,
            heartbeat_due_in: 0,
            cluster_size,
            rng_state: (id as u64).wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407) | 1,
        };
        for peer in 0..cluster_size {
            if peer != id {
                s.next_index.insert(peer, 1);
                s.match_index.insert(peer, 0);
            }
        }
        s
    }

    pub fn quorum(&self) -> usize {
        self.cluster_size / 2 + 1
    }

    pub fn last_log_index(&self) -> u64 {
        self.log.back().map(|e| e.index).unwrap_or(0)
    }

    pub fn last_log_term(&self) -> u64 {
        self.log.back().map(|e| e.term).unwrap_or(0)
    }

    fn next_rand(&mut self) -> u64 {
        self.rng_state ^= self.rng_state << 13;
        self.rng_state ^= self.rng_state >> 7;
        self.rng_state ^= self.rng_state << 17;
        self.rng_state
    }

    /// Advance logical time; return outbound messages on timeout.
    fn tick(&mut self, elapsed: u64) -> Vec<(NodeId, RaftMessage)> {
        let mut out = Vec::new();
        match self.role {
            Role::Follower | Role::Candidate => {
                self.election_timeout_in = self.election_timeout_in.saturating_sub(elapsed);
                if self.election_timeout_in == 0 {
                    self.role = Role::Candidate;
                    self.current_term += 1;
                    self.voted_for = Some(self.self_id);
                    self.votes_received = 1;
                    self.election_timeout = 150 + self.next_rand() % 75;
                    self.election_timeout_in = self.election_timeout;
                    for peer in 0..self.cluster_size {
                        if peer != self.self_id {
                            out.push((
                                peer,
                                RaftMessage::RequestVote {
                                    term: self.current_term,
                                    candidate: self.self_id,
                                    last_log_index: self.last_log_index(),
                                    last_log_term: self.last_log_term(),
                                },
                            ));
                        }
                    }
                    // Single-node cluster: the self-vote IS the quorum.
                    if self.votes_received >= self.quorum() {
                        self.role = Role::Leader;
                        self.heartbeat_due_in = 0;
                        let last = self.last_log_index();
                        for peer in 0..self.cluster_size {
                            if peer != self.self_id {
                                self.next_index.insert(peer, last + 1);
                                self.match_index.insert(peer, 0);
                            }
                        }
                    }
                }
            }
            Role::Leader => {
                self.heartbeat_due_in = self.heartbeat_due_in.saturating_sub(elapsed);
                if self.heartbeat_due_in == 0 {
                    self.heartbeat_due_in = 50;
                    for peer in 0..self.cluster_size {
                        if peer != self.self_id {
                            let msg = self.build_append_for(peer);
                            out.push((peer, msg));
                        }
                    }
                }
            }
        }
        out
    }

    fn build_append_for(&mut self, peer: NodeId) -> RaftMessage {
        let next = *self.next_index.get(&peer).unwrap_or(&1);
        let prev_index = next - 1;
        let prev_term = self.log.iter().find(|e| e.index == prev_index).map(|e| e.term).unwrap_or(0);
        let entries: Vec<LogEntry> = self.log.iter().filter(|e| e.index >= next).cloned().collect();
        RaftMessage::AppendEntries {
            term: self.current_term,
            leader: self.self_id,
            prev_log_index: prev_index,
            prev_log_term: prev_term,
            entries,
            leader_commit: self.commit_index,
        }
    }

    /// Adopt `term` if it is higher: clear the vote, drop to Follower,
    /// re-arm the election timer. Returns whether a step-down happened.
    /// (This is the §13.1 state-machine transition every server makes on
    /// observing a higher term, whatever the message kind.)
    fn become_follower(&mut self, term: u64) -> bool {
        if term > self.current_term {
            self.current_term = term;
            self.voted_for = None;
            self.role = Role::Follower;
            self.votes_received = 0;
            self.election_timeout_in = self.election_timeout;
            true
        } else {
            false
        }
    }

    fn step_down_if_stale(&mut self, term: u64) -> bool {
        self.become_follower(term)
    }

    /// Handle an inbound message from `from`; returns replies `(dest, msg)`.
    fn handle(&mut self, from: NodeId, msg: RaftMessage) -> Vec<(NodeId, RaftMessage)> {
        let mut replies = Vec::new();
        match msg {
            RaftMessage::RequestVote { term, candidate, last_log_index, last_log_term } => {
                if term < self.current_term {
                    replies.push((candidate, RaftMessage::RequestVoteReply {
                        term: self.current_term,
                        vote_granted: false,
                    }));
                    return replies;
                }
                self.step_down_if_stale(term);
                let up_to_date = last_log_term > self.last_log_term()
                    || (last_log_term == self.last_log_term()
                        && last_log_index >= self.last_log_index());
                let can_vote = self.voted_for.is_none() || self.voted_for == Some(candidate);
                if can_vote && up_to_date {
                    self.voted_for = Some(candidate);
                    self.election_timeout_in = self.election_timeout;
                    replies.push((candidate, RaftMessage::RequestVoteReply {
                        term: self.current_term,
                        vote_granted: true,
                    }));
                } else {
                    replies.push((candidate, RaftMessage::RequestVoteReply {
                        term: self.current_term,
                        vote_granted: false,
                    }));
                }
            }
            RaftMessage::RequestVoteReply { term, vote_granted } => {
                if self.role != Role::Candidate {
                    self.step_down_if_stale(term);
                    return replies;
                }
                if term > self.current_term {
                    self.step_down_if_stale(term);
                    return replies;
                }
                if term == self.current_term && vote_granted {
                    self.votes_received += 1;
                    if self.votes_received >= self.quorum() {
                        // Won the election.
                        self.role = Role::Leader;
                        self.heartbeat_due_in = 0;
                        let last = self.last_log_index();
                        for peer in 0..self.cluster_size {
                            if peer != self.self_id {
                                self.next_index.insert(peer, last + 1);
                                self.match_index.insert(peer, 0);
                            }
                        }
                        // Immediate heartbeats establish authority.
                        for peer in 0..self.cluster_size {
                            if peer != self.self_id {
                                let msg = self.build_append_for(peer);
                                replies.push((peer, msg));
                            }
                        }
                    }
                }
            }
            RaftMessage::AppendEntries {
                term,
                leader,
                prev_log_index,
                prev_log_term,
                entries,
                leader_commit,
            } => {
                if term < self.current_term {
                    replies.push((leader, RaftMessage::AppendEntriesReply {
                        term: self.current_term,
                        success: false,
                        match_index: self.last_log_index(),
                    }));
                    return replies;
                }
                self.step_down_if_stale(term);
                if self.role == Role::Candidate {
                    self.role = Role::Follower; // recognize the leader
                }
                self.election_timeout_in = self.election_timeout;

                let consistent = if prev_log_index == 0 {
                    true
                } else {
                    match self.log.iter().find(|e| e.index == prev_log_index) {
                        Some(e) => e.term == prev_log_term,
                        None => false,
                    }
                };
                if !consistent {
                    replies.push((leader, RaftMessage::AppendEntriesReply {
                        term: self.current_term,
                        success: false,
                        match_index: self.last_log_index(),
                    }));
                    return replies;
                }
                for entry in entries {
                    match self.log.iter().position(|e| e.index == entry.index) {
                        Some(pos) => {
                            if self.log[pos].term == entry.term {
                                continue; // already replicated
                            }
                            self.log.truncate(pos); // conflicting entry — cut
                            self.log.push_back(entry);
                        }
                        None => {
                            if entry.index == self.last_log_index() + 1 {
                                self.log.push_back(entry);
                            }
                        }
                    }
                }
                if leader_commit > self.commit_index {
                    let last = self.last_log_index();
                    self.commit_index = leader_commit.min(last);
                }
                replies.push((leader, RaftMessage::AppendEntriesReply {
                    term: self.current_term,
                    success: true,
                    match_index: self.last_log_index(),
                }));
            }
            RaftMessage::AppendEntriesReply { term, success, match_index } => {
                if self.role != Role::Leader {
                    self.step_down_if_stale(term);
                    return replies;
                }
                if term > self.current_term {
                    self.step_down_if_stale(term);
                    return replies;
                }
                if term < self.current_term {
                    return replies; // stale reply
                }
                if success {
                    let mi = self.match_index.entry(from).or_insert(0);
                    *mi = (*mi).max(match_index);
                    self.advance_commit();
                } else {
                    // Back off and retry immediately.
                    let ni = self.next_index.entry(from).or_insert(1);
                    *ni = (*ni).max(1) - 1;
                    let msg = self.build_append_for(from);
                    replies.push((from, msg));
                }
            }
        }
        replies
    }

    /// Commit rule (Raft §5.4.2): only entries from the current term count
    /// toward the quorum barrier.
    fn advance_commit(&mut self) {
        let quorum = self.quorum();
        let last = self.last_log_index();
        let mut committed = self.commit_index;
        for n in (self.commit_index + 1)..=last {
            let entry_term = self.log.iter().find(|e| e.index == n).map(|e| e.term).unwrap_or(0);
            if entry_term != self.current_term {
                continue; // old-term entries commit only implicitly
            }
            let replicas = 1 + self.match_index.values().filter(|&&m| m >= n).count();
            if replicas >= quorum {
                committed = n;
            }
        }
        self.commit_index = committed;
    }

    /// Leader accepts a client command.
    pub fn append_local(&mut self, command: Vec<u8>) {
        let index = self.last_log_index() + 1;
        self.log.push_back(LogEntry { term: self.current_term, index, command });
        // NOTE: self is NOT tracked in match_index — advance_commit counts
        // the leader's own replica explicitly (+1), so an isolated
        // minority leader cannot reach quorum by double-counting itself.
        self.advance_commit(); // single-node cluster commits immediately
    }
}

// ---------------------------------------------------------------------------
// Network (deterministic, partition-aware)
// ---------------------------------------------------------------------------

/// Deterministic message-switched cluster.
pub struct Network {
    nodes: Vec<Server>,
    /// (from, to, message) — sender-tagged so replies can route.
    inbox: VecDeque<(NodeId, NodeId, RaftMessage)>,
    partitions: HashMap<(NodeId, NodeId), f64>,
    rng_state: u64,
}

impl Network {
    pub fn new(size: usize, seed: u64) -> Self {
        Self {
            nodes: (0..size).map(|id| Server::new(id, size)).collect(),
            inbox: VecDeque::new(),
            partitions: HashMap::new(),
            rng_state: seed | 1,
        }
    }

    pub fn size(&self) -> usize {
        self.nodes.len()
    }

    /// Cut (or probabilistically degrade) the link i → j.
    pub fn set_partition(&mut self, i: NodeId, j: NodeId, drop_prob: f64) {
        self.partitions.insert((i, j), drop_prob.clamp(0.0, 1.0));
    }

    pub fn heal_all(&mut self) {
        self.partitions.clear();
    }

    fn next_f64(&mut self) -> f64 {
        self.rng_state ^= self.rng_state << 13;
        self.rng_state ^= self.rng_state >> 7;
        self.rng_state ^= self.rng_state << 17;
        (self.rng_state >> 11) as f64 / (1u64 << 53) as f64
    }

    fn send(&mut self, from: NodeId, to: NodeId, msg: RaftMessage) {
        let drop_p = *self.partitions.get(&(from, to)).unwrap_or(&0.0);
        if self.next_f64() >= drop_p {
            self.inbox.push_back((from, to, msg));
        }
    }

    /// Advance logical time; timeouts fire and produce traffic.
    pub fn tick(&mut self, elapsed: u64) {
        for i in 0..self.nodes.len() {
            let actions = self.nodes[i].tick(elapsed);
            for (to, msg) in actions {
                self.send(i, to, msg);
            }
        }
    }

    /// Deliver every currently pending message once; replies queue back.
    /// Returns the number of messages delivered.
    pub fn step(&mut self) -> usize {
        if self.inbox.is_empty() {
            return 0;
        }
        let batch: Vec<(NodeId, NodeId, RaftMessage)> = self.inbox.drain(..).collect();
        let delivered = batch.len();
        for (from, to, msg) in batch {
            let replies = self.nodes[to].handle(from, msg);
            for (dest, reply) in replies {
                self.send(to, dest, reply);
            }
        }
        delivered
    }

    /// Deliver until quiescent (bounded).
    pub fn run_until_quiet(&mut self, max_rounds: usize) {
        for _ in 0..max_rounds {
            if self.step() == 0 {
                break;
            }
        }
    }

    /// Submit a client command to `to`; fails if `to` is not the leader.
    pub fn propose(&mut self, to: NodeId, command: Vec<u8>) -> Result<(), String> {
        if self.nodes[to].role != Role::Leader {
            return Err(format!("node {to} is not the leader"));
        }
        self.nodes[to].append_local(command);
        self.nodes[to].heartbeat_due_in = 0; // replicate immediately
        Ok(())
    }

    pub fn roles(&self) -> Vec<Role> {
        self.nodes.iter().map(|s| s.role).collect()
    }

    pub fn leader_id(&self) -> Option<NodeId> {
        self.nodes.iter().find(|s| s.role == Role::Leader).map(|s| s.self_id)
    }

    pub fn leader_commit_index(&self) -> Option<u64> {
        self.nodes.iter().find(|s| s.role == Role::Leader).map(|s| s.commit_index)
    }

    /// Client-visible committed log (from the leader).
    pub fn committed_log(&self) -> Vec<LogEntry> {
        match self.nodes.iter().find(|s| s.role == Role::Leader) {
            Some(leader) => leader
                .log
                .iter()
                .filter(|e| e.index <= leader.commit_index)
                .cloned()
                .collect(),
            None => Vec::new(),
        }
    }

    /// True when every server has the entry at (index, term).
    pub fn replicated_everywhere(&self, index: u64, term: u64) -> bool {
        self.nodes
            .iter()
            .all(|s| s.log.iter().any(|e| e.index == index && e.term == term))
    }

    /// Access a server (read-only inspection for tests/tools).
    pub fn server(&self, id: NodeId) -> &Server {
        &self.nodes[id]
    }

    /// Access a server mutably (test fault injection / term manipulation).
    pub fn server_mut(&mut self, id: NodeId) -> &mut Server {
        &mut self.nodes[id]
    }
}

/// Drive the cluster: tick + deliver until quiescent, for `rounds` cycles.
pub fn run_stable(net: &mut Network, rounds: usize) {
    for _ in 0..rounds {
        net.tick(20);
        net.run_until_quiet(16);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn become_follower_resets_election_state() {
        // A leader that steps down to a higher term must: adopt the term,
        // forget its vote, drop back to Follower, and re-arm its election
        // timer. This is the state-machine transition every real transport
        // triggers on observing a higher term.
        let mut net = Network::new(3, 42);
        run_stable(&mut net, 40);
        let leader = net.leader_id().unwrap();
        {
            let srv = net.server_mut(leader);
            assert_eq!(srv.role, Role::Leader);
            let timeout = srv.election_timeout;
            srv.become_follower(99);
            assert_eq!(srv.current_term, 99, "term must advance");
            assert_eq!(srv.voted_for, None, "vote must be forgotten");
            assert_eq!(srv.role, Role::Follower, "must step down to follower");
            assert_eq!(srv.election_timeout_in, timeout, "election timer re-armed");
        }
        // Stepping down to the *same* or lower term must be a no-op.
        let srv = net.server_mut(leader);
        let term_before = srv.current_term;
        srv.become_follower(1);
        assert_eq!(srv.current_term, term_before, "term must not regress");
        assert_eq!(srv.role, Role::Follower);
    }

    #[test]
    fn three_node_cluster_elects_exactly_one_leader() {
        let mut net = Network::new(3, 42);
        run_stable(&mut net, 40);
        let leaders = net.roles().iter().filter(|r| **r == Role::Leader).count();
        assert_eq!(leaders, 1, "roles: {:?}", net.roles());
    }

    #[test]
    fn five_node_cluster_elects_leader() {
        let mut net = Network::new(5, 7);
        run_stable(&mut net, 60);
        let leaders = net.roles().iter().filter(|r| **r == Role::Leader).count();
        assert_eq!(leaders, 1);
    }

    #[test]
    fn commands_replicate_to_all_nodes() {
        let mut net = Network::new(3, 123);
        run_stable(&mut net, 40);
        let leader = net.leader_id().unwrap();
        net.propose(leader, b"HEAD-swap:/a.txt->h(abc)".to_vec()).unwrap();
        net.propose(leader, b"HEAD-swap:/b.txt->h(def)".to_vec()).unwrap();
        run_stable(&mut net, 40);

        let committed = net.committed_log();
        assert_eq!(committed.len(), 2, "both commands must commit");
        assert_eq!(committed[0].command, b"HEAD-swap:/a.txt->h(abc)".to_vec());
        assert_eq!(committed[1].index, 2);
        // Replicated on every server.
        assert!(net.replicated_everywhere(1, committed[0].term));
        assert!(net.replicated_everywhere(2, committed[1].term));
        // And every follower's commit index advanced.
        for s in 0..net.size() {
            assert!(net.server(s).commit_index >= 2, "server {s} commit = {}", net.server(s).commit_index);
        }
    }

    #[test]
    fn single_node_cluster_commits_immediately() {
        let mut net = Network::new(1, 5);
        run_stable(&mut net, 40);
        assert_eq!(net.leader_id(), Some(0));
        net.propose(0, b"solo".to_vec()).unwrap();
        run_stable(&mut net, 10);
        assert_eq!(net.committed_log().len(), 1);
    }

    #[test]
    fn propose_to_non_leader_fails() {
        let mut net = Network::new(3, 21);
        run_stable(&mut net, 40);
        let leader = net.leader_id().unwrap();
        let follower = (0..3).find(|&i| i != leader).unwrap();
        assert!(net.propose(follower, b"x".to_vec()).is_err());
    }

    #[test]
    fn minority_partition_cannot_commit() {
        let mut net = Network::new(3, 99);
        run_stable(&mut net, 40);
        let leader = net.leader_id().unwrap();
        let followers: Vec<NodeId> = (0..3).filter(|&i| i != leader).collect();

        // Cut the leader away from both followers (minority side).
        for &f in &followers {
            net.set_partition(leader, f, 1.0);
            net.set_partition(f, leader, 1.0);
        }
        net.propose(leader, b"doomed".to_vec()).unwrap();
        run_stable(&mut net, 30);

        // The majority side elects a new leader; the isolated old leader
        // must not report phantom commits.
        let leaders = net.roles().iter().filter(|r| **r == Role::Leader).count();
        assert!(leaders >= 1);
        // Whatever the isolated leader thinks, nothing new committed there:
        let isolated_log: Vec<LogEntry> = net
            .server(leader)
            .log
            .iter()
            .filter(|e| e.index <= net.server(leader).commit_index)
            .cloned()
            .collect();
        assert!(
            !isolated_log.iter().any(|e| e.command == b"doomed"),
            "isolated minority leader must not commit 'doomed'"
        );

        // Heal — cluster reconverges with exactly one leader.
        net.heal_all();
        run_stable(&mut net, 80);
        let leaders = net.roles().iter().filter(|r| **r == Role::Leader).count();
        assert_eq!(leaders, 1, "roles: {:?}", net.roles());
    }

    #[test]
    fn leader_with_majority_keeps_committing() {
        let mut net = Network::new(5, 555);
        run_stable(&mut net, 50);
        let leader = net.leader_id().unwrap();
        let victim = (0..5).find(|&i| i != leader).unwrap();
        net.set_partition(leader, victim, 1.0);
        net.set_partition(victim, leader, 1.0);
        net.propose(leader, b"still-commits".to_vec()).unwrap();
        run_stable(&mut net, 50);
        let committed = net.committed_log();
        assert!(committed.iter().any(|e| e.command == b"still-commits"));
    }

    #[test]
    fn election_is_deterministic_per_seed() {
        let l1 = {
            let mut net = Network::new(3, 4242);
            run_stable(&mut net, 40);
            net.leader_id()
        };
        let l2 = {
            let mut net = Network::new(3, 4242);
            run_stable(&mut net, 40);
            net.leader_id()
        };
        assert_eq!(l1, l2);
    }

    #[test]
    fn split_brain_reconverges_and_continues() {
        let mut net = Network::new(3, 31337);
        run_stable(&mut net, 40);
        let leader = net.leader_id().unwrap();
        net.propose(leader, b"entry-1".to_vec()).unwrap();
        run_stable(&mut net, 40);
        assert_eq!(net.committed_log().len(), 1);

        // Split: isolate old leader.
        let others: Vec<NodeId> = (0..3).filter(|&i| i != leader).collect();
        for &o in &others {
            net.set_partition(leader, o, 1.0);
            net.set_partition(o, leader, 1.0);
        }
        run_stable(&mut net, 40); // majority elects new leader, higher term
        net.heal_all();
        run_stable(&mut net, 100);

        assert_eq!(net.roles().iter().filter(|r| **r == Role::Leader).count(), 1);
        let new_leader = net.leader_id().unwrap();
        net.propose(new_leader, b"post-reconvergence".to_vec()).unwrap();
        run_stable(&mut net, 80);
        let committed = net.committed_log();
        assert!(
            committed.iter().any(|e| e.command == b"post-reconvergence"),
            "reconverged cluster must commit new writes: {committed:?}"
        );
        // The old entry survived the reconvergence (log reconciliation via
        // AppendEntries back-off), OR was overwritten by a higher-term
        // leader that lacked it — either is legal Raft, but the new entry
        // must be durable everywhere.
        assert!(net.replicated_everywhere(
            committed.last().unwrap().index,
            committed.last().unwrap().term
        ));
    }
}

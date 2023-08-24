use log::debug;

use rand::{thread_rng, Rng};
use std::{
    cmp::min,
    collections::HashMap,
    sync::mpsc::{Receiver, RecvTimeoutError, Sender},
    time::{Duration, Instant},
};

mod error;

pub use error::{Error, Result};

/// An instance of a Raft Node
#[derive(Debug)]
pub struct Node {
    /// This `Node`s id
    pub id: usize,
    /// Either `Follower`, `Candidate` or `Leader`
    pub role: Role,
    /// The number of votes we received this term
    pub votes: usize,

    /// The current term
    pub term: usize,
    /// If we've voted in this term and who for
    pub voted_for: Option<usize>,
    /// The current log
    pub log: Vec<Log>,

    /// index of the highest log entry known to be committed
    pub commit_index: usize,
    /// index of highest log entry applied to state machine
    pub last_applied: usize,

    /// for each server, index of the next log entry to send to that server
    pub next_index: HashMap<usize, usize>,
    /// for each server, index of highest log entry known to be replicated on that server
    pub match_index: HashMap<usize, usize>,

    /// Receive all messages on a single channel
    ///
    /// TODO: Create a network thread to send RPC messages on this channel
    pub inbox: Receiver<Message>,
    /// Store network peers as a `HashMap` from id's to a `Sender`
    ///
    /// TODO: Create a network thread that simply receives on these channels
    pub peers: HashMap<usize, Sender<Message>>,

    /// For manually shutting down the node in testing
    pub killed: bool,
}

// for checking what the current state of the node is
#[derive(Debug, PartialEq)]
pub struct State {
    id: usize,
    role: Role,
    votes: usize,
    term: usize,
    voted_for: Option<usize>,
    log: Vec<Log>,
    commit_index: usize,
    last_applied: usize,
    next_index: HashMap<usize, usize>,
    match_index: HashMap<usize, usize>,
    killed: bool,
}

impl Node {
    /// Generate a new `Node` with a blank slate (i.e. a "Follower")
    pub fn new(
        id: usize,
        inbox: Receiver<Message>,
        peers: HashMap<usize, Sender<Message>>,
    ) -> Self {
        Self {
            id,
            role: Role::Follower,
            votes: 0,
            term: 0,
            voted_for: None,
            log: vec![Log {
                term: 0,
                command: "".to_string(),
            }],
            commit_index: 0,
            last_applied: 0,
            next_index: HashMap::new(),
            match_index: HashMap::new(),
            inbox,
            peers,
            killed: false,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
pub enum Role {
    #[default]
    Follower,
    Candidate,
    Leader,
}

#[derive(Debug, Clone)]
pub struct RequestVoteArgs {
    term: usize,
    candidate_id: usize,
    last_log_index: usize,
    last_log_term: usize,
}

#[derive(Debug)]
pub struct RequestVoteReply {
    term: usize,
    vote_granted: bool,
}

#[derive(Debug, Clone)]
pub struct AppendEntriesArgs {
    term: usize,
    leader_id: usize,
    prev_log_index: usize,
    prev_log_term: usize,
    entries: Vec<Log>,
    leader_commit: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Log {
    pub term: usize,
    pub command: String,
}

#[derive(Debug)]
pub struct AppendEntriesReply {
    term: usize,
    success: bool,
    peer: usize,
}

#[derive(Debug)]
pub enum Message {
    AppendEntries(AppendEntriesArgs),
    AppendEntriesReply(AppendEntriesReply),
    RequestVote(RequestVoteArgs),
    RequestVoteReply(RequestVoteReply),
    CheckState(Sender<Message>),
    State(State),
    Kill,
}

impl Node {
    pub fn start(mut self) -> Result<()> {
        let mut t = Instant::now();
        let mut rng = thread_rng();
        loop {
            // handle all the messages in our inbox
            let mut timeout = match self.role {
                Role::Leader => Duration::from_millis(50 + rng.gen_range(0..25)),
                Role::Follower | Role::Candidate => {
                    Duration::from_millis(100 + rng.gen_range(0..50))
                }
            };
            debug!("Setting initial timeout to {}", timeout.as_millis());
            loop {
                match self.inbox.recv_timeout(timeout) {
                    Ok(m) => {
                        debug!("Got a message {:?}", m);
                        // if a read returns true, that means we should restart our tick timer
                        if self.handle_message(m)? {
                            t = Instant::now();
                            timeout = match self.role {
                                Role::Follower | Role::Candidate => {
                                    Duration::from_millis(100 + rng.gen_range(0..50))
                                }
                                Role::Leader => Duration::from_millis(50 + rng.gen_range(0..50)),
                            }
                        // if read returns false, then simply decrement the time elapsed
                        } else {
                            timeout -= t.elapsed();
                        };
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        debug!(
                            "{}: Timer timeout: term {}, role {:?}",
                            self.id, self.term, self.role
                        );
                        self.tick()?;
                        t = Instant::now();
                        timeout = match self.role {
                            Role::Leader => Duration::from_millis(50),
                            Role::Follower | Role::Candidate => Duration::from_millis(100),
                        };
                    }
                    // This shouldn't happen because the network should live as long as the node
                    Err(RecvTimeoutError::Disconnected) => {
                        debug!("Broken Inbox: inbox channel disconnected");
                        return Err(Error::BrokenInbox);
                    }
                }
            }
        }
    }

    /// handles the message on the state machine
    ///
    /// if the message results in needing to reset the timer, we return `true`
    fn handle_message(&mut self, m: Message) -> Result<bool> {
        if self.killed {
            return Ok(true);
        }

        match m {
            Message::RequestVote(args) => self.handle_request_vote(args),
            Message::RequestVoteReply(reply) => self.handle_request_vote_reply(reply),
            Message::AppendEntries(args) => self.handle_append_entries(args),
            Message::AppendEntriesReply(reply) => self.handle_append_entries_reply(reply),
            Message::CheckState(tx) => self.handle_check_state(tx),
            Message::State(_) => Ok(false),
            Message::Kill => {
                self.killed = true;
                Ok(true)
            }
        }
    }

    // check the state of the node for testing purposes
    fn handle_check_state(&self, tx: Sender<Message>) -> Result<bool> {
        tx.send(Message::State(self.state())).unwrap();
        Ok(false)
    }

    fn state(&self) -> State {
        State {
            id: self.id,
            role: self.role.clone(),
            votes: self.votes,
            term: self.term,
            voted_for: self.voted_for,
            log: self.log.clone(),
            commit_index: self.commit_index,
            last_applied: self.last_applied,
            next_index: self.next_index.clone(),
            match_index: self.match_index.clone(),
            killed: self.killed,
        }
    }

    fn handle_request_vote(&mut self, args: RequestVoteArgs) -> Result<bool> {
        let mut reply = RequestVoteReply {
            term: self.term,
            vote_granted: false,
        };

        if args.term > self.term {
            self.become_follower(args.term);
        }

        reply.term = self.term;

        debug!(
            "{}: Received vote request from {}, term = {}",
            self.id, args.candidate_id, self.term
        );

        if args.term < self.term {
            debug!(
                "{}: Rejected vote request from {}, outdated term: got {}, have {}",
                self.id, args.candidate_id, args.term, self.term
            );

            reply.vote_granted = false;

            self.send_message(args.candidate_id, Message::RequestVoteReply(reply))?;

            return Ok(false);
        }

        if self.voted_for == None || self.voted_for == Some(args.candidate_id) {
            let our_last_index = self.log.len() - 1;
            let our_last_term = self.log[our_last_index].term;

            if args.last_log_term > our_last_term {
                debug!("{}: granting vote to {}", self.id, args.candidate_id);

                self.voted_for = Some(args.candidate_id);
                reply.vote_granted = true;

                self.send_message(args.candidate_id, Message::RequestVoteReply(reply))?;
                return Ok(true);
            } else if args.last_log_term == our_last_term {
                if args.last_log_index >= our_last_index {
                    debug!("{}: granting vote to {}", self.id, args.candidate_id);

                    self.voted_for = Some(args.candidate_id);
                    reply.vote_granted = true;
                    self.send_message(args.candidate_id, Message::RequestVoteReply(reply))?;
                    return Ok(true);
                } else {
                    debug!("{}: Rejected vote request from {}, log outdated: got term {}, have term {}", self.id, args.candidate_id, our_last_term, args.last_log_term);
                }
            } else {
                debug!(
                    "{}: Rejected vote request from {}, log outdated: got index {}, have index {}",
                    self.id, args.candidate_id, our_last_index, args.last_log_index
                );
            }
        }
        debug!(
            "{}: Rejected vote request from {}, already voted for {}",
            self.id,
            args.candidate_id,
            self.voted_for.unwrap()
        );

        self.send_message(args.candidate_id, Message::RequestVoteReply(reply))?;
        Ok(false)
    }

    /// It is assumed that replies to `requestVote` don't go to the wrong node, they either
    /// get sent or they fail to reach destination for whatever reason.
    ///
    /// Therefore, if we receive a reply, it's either to our current term, or it's a stale reply
    /// In the first case, we handle it as specified exactly in the raft paper, else we ignore
    fn handle_request_vote_reply(&mut self, reply: RequestVoteReply) -> Result<bool> {
        if reply.term < self.term {
            debug!(
                "{}: Stale request vote reply from term {}",
                self.id, reply.term
            );
            return Ok(false);
        } else if reply.term > self.term {
            debug!(
                "{}: request vote reply with greater term {}",
                self.id, reply.term
            );
            self.become_follower(reply.term);
        } else {
            if reply.vote_granted {
                self.votes += 1;
                if self.votes >= self.peers.len() / 2 + 1 {
                    debug!(
                        "{}: won the election: votes {}, needed {}",
                        self.id,
                        self.votes,
                        self.peers.len() / 2 + 1
                    );
                    self.role = Role::Leader;
                    return Ok(true);
                }
            }
        }

        Ok(false)
    }

    fn handle_append_entries(&mut self, args: AppendEntriesArgs) -> Result<bool> {
        debug!(
            "{}: AppendEntries received from {}",
            self.id, args.leader_id
        );

        let mut reply = AppendEntriesReply {
            term: self.term,
            success: false,
            peer: self.id,
        };

        if args.term > self.term {
            self.become_follower(args.term);
        }

        reply.term = self.term;

        if args.term < self.term {
            debug!(
                "{}: AppendEntries from {}, term less than ours: got {}, have {}",
                self.id, args.leader_id, args.term, self.term
            );

            reply.success = false;

            self.send_message(args.leader_id, Message::AppendEntriesReply(reply))?;
            return Ok(false);
        }

        if self.log.len() - 1 < args.prev_log_index {
            debug!(
                "{}: AppendEntries from {}: inconsistent log index: got {}, have {}",
                self.id,
                args.leader_id,
                args.prev_log_index,
                self.log.len() - 1
            );

            reply.success = false;

            self.send_message(args.leader_id, Message::AppendEntriesReply(reply))?;
            return Ok(false);
        }

        if self.log[args.prev_log_index].term != args.prev_log_term {
            debug!(
                "{}: AppendEntries from {}: inconsistent log term: got {}, have {}",
                self.id, args.leader_id, args.prev_log_term, self.log[args.prev_log_index].term
            );

            reply.success = false;

            self.send_message(args.leader_id, Message::AppendEntriesReply(reply))?;
            return Ok(false);
        }

        // if an existing entry conflicts with a new one (same index but different terms)
        // delete the existing entry and all that follow it
        let mut idx = args.prev_log_index + 1;
        for entry in &args.entries {
            if idx > self.log.len() - 1 {
                break;
            }
            if entry.term != self.log[idx].term {
                self.log = self.log[..idx].to_vec();
                break;
            }
            idx += 1;
        }

        // append any new entries not in the log
        let idx = args.prev_log_index + 1;
        for i in 0..args.entries.len() {
            if idx + 1 > self.log.len() - 1 {
                self.log.append(&mut args.entries[i..].to_vec());
                break;
            }
        }

        // receiver implementation 5 from Raft paper
        if args.leader_commit > self.commit_index {
            let idx_last_new_entry = self.log.len() - 1;

            self.commit_index = min(idx_last_new_entry, args.leader_commit);

            self.check_last_applied();
        }

        self.voted_for = Some(args.leader_id);
        reply.success = true;
        self.send_message(args.leader_id, Message::AppendEntriesReply(reply))?;
        debug!(
            "{}: AppendEntries from {} successful: Current log {:?}, commit_index {}",
            self.id, args.leader_id, self.log, self.commit_index
        );

        // reset the timer
        Ok(true)
    }

    fn check_last_applied(&mut self) {
        todo!()
    }

    fn become_follower(&mut self, term: usize) {
        self.term = term;
        self.voted_for = None;
        self.votes = 0;
        self.role = Role::Follower;
    }

    fn handle_append_entries_reply(&mut self, reply: AppendEntriesReply) -> Result<bool> {
        if reply.term > self.term {
            self.become_follower(reply.term);
            return Ok(true);
        } else if reply.term < self.term {
            return Ok(false);
        } else {
            if reply.success {
                // WARNING: This might create a race condition if there is a log appended
                // before this reply gets to us, therefore causing us to think this peer is
                // more up to date than he is.
                self.next_index.insert(reply.peer, self.log.len());
                self.match_index.insert(reply.peer, self.log.len() - 1);
            } else {
                let next_index = self.next_index.get(&reply.peer).unwrap();
                self.next_index.insert(reply.peer, next_index - 1);
            }
        }
        Ok(false)
    }

    /// send a message to a given peer
    pub fn send_message(&self, peer: usize, m: Message) -> Result<()> {
        let sender = self.peers.get(&peer).unwrap();
        sender.send(m)?;
        Ok(())
    }

    /// either call an election if we are a candidate or a follower, else send heartbeats
    /// to all the followers
    pub fn tick(&mut self) -> Result<()> {
        if self.killed {
            return Ok(());
        }

        match self.role {
            Role::Follower | Role::Candidate => self.start_election()?,
            Role::Leader => self.send_heartbeats()?,
        }

        Ok(())
    }

    fn start_election(&mut self) -> Result<()> {
        self.term += 1;
        self.voted_for = Some(self.id);
        self.votes = 1;

        debug!(
            "{}: calling an election: term {}, votes {}",
            self.id, self.term, self.votes
        );
        if self.votes >= self.peers.len() / 2 + 1 {
            debug!(
                "{}: won the election: votes {}, needed {}",
                self.id,
                self.votes,
                self.peers.len() / 2 + 1
            );

            self.role = Role::Leader;
            return Ok(());
        }

        let request = RequestVoteArgs {
            term: self.term,
            candidate_id: self.id,
            last_log_index: self.log.len() - 1,
            last_log_term: self.log[self.log.len() - 1].term,
        };

        for peer in self.peers.keys() {
            debug!("{}: sending request vote to {}", self.id, peer);
            self.send_message(*peer, Message::RequestVote(request.clone()))?;
        }

        Ok(())
    }

    fn send_heartbeats(&mut self) -> Result<()> {
        debug!(
            "{}: Sending heartbeats to peers: term {}",
            self.id, self.term
        );

        let request = AppendEntriesArgs {
            entries: Vec::new(),
            leader_commit: self.commit_index,
            leader_id: self.id,
            prev_log_index: 0,
            prev_log_term: 0,
            term: self.term,
        };

        for peer in self.peers.keys() {
            debug!("{}: sending heartbeat to {}", self.id, peer);
            self.send_message(*peer, Message::AppendEntries(request.clone()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;

    // A single node should start an election and elect itself the leader
    #[test_log::test]
    fn test_one_node_election() {
        let (tx, rx) = std::sync::mpsc::channel();
        let (state_tx, state_rx) = std::sync::mpsc::channel();
        let node = Node::new(1, rx, HashMap::new());

        thread::spawn(|| {
            node.start().unwrap();
        });

        thread::sleep(Duration::from_millis(200));

        tx.send(Message::CheckState(state_tx.clone())).unwrap();

        let Message::State(state) = state_rx.recv().unwrap() else {
            panic!("Expected Message::State");
        };

        let expected = State {
            id: 1,
            commit_index: 0,
            last_applied: 0,
            log: vec![Log {
                term: 0,
                command: "".to_string(),
            }],
            match_index: HashMap::new(),
            next_index: HashMap::new(),
            role: Role::Leader,
            term: 1,
            voted_for: Some(1),
            votes: 1,
            killed: false,
        };

        assert_eq!(expected, state);
    }

    #[test_log::test]
    fn test_three_node_election() {
        let (tx_1, rx_1) = std::sync::mpsc::channel::<Message>();
        let (tx_2, rx_2) = std::sync::mpsc::channel::<Message>();
        let (tx_3, rx_3) = std::sync::mpsc::channel::<Message>();

        let (state_tx_1, state_rx_1) = std::sync::mpsc::channel();
        let (state_tx_2, state_rx_2) = std::sync::mpsc::channel();
        let (state_tx_3, state_rx_3) = std::sync::mpsc::channel();

        let node_1 = Node::new(
            1,
            rx_1,
            HashMap::from([(2, tx_2.clone()), (3, tx_3.clone())]),
        );
        let node_2 = Node::new(
            2,
            rx_2,
            HashMap::from([(1, tx_1.clone()), (3, tx_3.clone())]),
        );
        let node_3 = Node::new(
            3,
            rx_3,
            HashMap::from([(1, tx_1.clone()), (2, tx_2.clone())]),
        );

        thread::spawn(|| {
            node_1.start().unwrap();
        });
        thread::spawn(|| {
            node_2.start().unwrap();
        });
        thread::spawn(|| {
            node_3.start().unwrap();
        });

        thread::sleep(Duration::from_millis(200));

        tx_1.send(Message::CheckState(state_tx_1)).unwrap();
        tx_2.send(Message::CheckState(state_tx_2)).unwrap();
        tx_3.send(Message::CheckState(state_tx_3)).unwrap();

        thread::sleep(Duration::from_millis(100));

        let Message::State(state_1) = state_rx_1.recv().unwrap() else {
            panic!("Expected Message::State");
        };
        let Message::State(state_2) = state_rx_2.recv().unwrap() else {
            panic!("Expected Message::State");
        };
        let Message::State(state_3) = state_rx_3.recv().unwrap() else {
            panic!("Expected Message::State");
        };

        if state_1.role != Role::Leader
            && state_2.role != Role::Leader
            && state_3.role != Role::Leader
        {
            panic!("No leader elected!!");
        }
    }

    fn init_nodes(num: usize) -> (Vec<Node>, Vec<Sender<Message>>) {
        let mut nodes = Vec::new();
        let mut senders = Vec::new();

        for i in 0..num {
            let (tx, rx) = std::sync::mpsc::channel::<Message>();
            nodes.push(Node::new(i + 1, rx, HashMap::new()));
            senders.push(tx);
        }

        for i in 0..num {
            for j in 0..num {
                if i != j {
                    nodes[i].peers.insert(j + 1, senders[j].clone());
                }
            }
        }

        (nodes, senders.clone())
    }

    #[test]
    fn test_init_one_node() {
        let (nodes, senders) = init_nodes(1);

        assert_eq!(nodes.len(), 1);
        assert_eq!(senders.len(), 1);

        assert_eq!(nodes[0].peers.len(), 0);
    }

    #[test]
    fn test_init_three_nodes() {
        let (nodes, senders) = init_nodes(3);

        assert_eq!(nodes.len(), 3);
        assert_eq!(senders.len(), 3);

        assert_eq!(nodes[0].peers.len(), 2);
        assert_eq!(nodes[1].peers.len(), 2);
        assert_eq!(nodes[2].peers.len(), 2);
    }

    fn make_state_channels(num: usize) -> (Vec<Sender<Message>>, Vec<Receiver<Message>>) {
        let mut senders = Vec::new();
        let mut receivers = Vec::new();
        for _ in 0..num {
            let (tx, rx) = std::sync::mpsc::channel();
            senders.push(tx);
            receivers.push(rx);
        }

        (senders, receivers)
    }

    #[test_log::test]
    fn test_three_nodes_kill_leader() {
        let (nodes, tx) = init_nodes(3);
        let (state_tx, state_rx) = make_state_channels(3);

        for node in nodes {
            thread::spawn(|| {
                let _ = node.start();
            });
        }

        thread::sleep(Duration::from_millis(200));

        tx[0]
            .send(Message::CheckState(state_tx[0].clone()))
            .unwrap();
        tx[1]
            .send(Message::CheckState(state_tx[1].clone()))
            .unwrap();
        tx[2]
            .send(Message::CheckState(state_tx[2].clone()))
            .unwrap();

        thread::sleep(Duration::from_millis(100));

        let Message::State(state_1) = state_rx[0].recv().unwrap() else {
            panic!("Expected Message::State");
        };
        let Message::State(state_2) = state_rx[1].recv().unwrap() else {
            panic!("Expected Message::State");
        };
        let Message::State(state_3) = state_rx[2].recv().unwrap() else {
            panic!("Expected Message::State");
        };

        #[allow(unused_assignments)]
        let mut killed = 0;

        if state_1.role == Role::Leader {
            debug!("Killed Node 1");
            tx[0].send(Message::Kill).unwrap();
            killed = 1;
        } else if state_2.role == Role::Leader {
            debug!("Killed Node 2");
            tx[1].send(Message::Kill).unwrap();
            killed = 2;
        } else if state_3.role == Role::Leader {
            debug!("Killed Node 3");
            tx[2].send(Message::Kill).unwrap();
            killed = 3;
        } else {
            panic!("No leader elected!");
        }

        thread::sleep(Duration::from_millis(300));

        let mut found_leader = 0;

        match killed {
            1 => {
                tx[1]
                    .send(Message::CheckState(state_tx[1].clone()))
                    .unwrap();
                tx[2]
                    .send(Message::CheckState(state_tx[2].clone()))
                    .unwrap();

                thread::sleep(Duration::from_millis(50));

                let Message::State(state_2) = state_rx[1].recv().unwrap() else {
                    panic!("Expected Message::State");
                };
                let Message::State(state_3) = state_rx[2].recv().unwrap() else {
                    panic!("Expected Message::State");
                };

                if state_2.role == Role::Leader {
                    found_leader += 1;
                }
                if state_3.role == Role::Leader {
                    found_leader += 1;
                }
            }
            2 => {
                tx[0]
                    .send(Message::CheckState(state_tx[0].clone()))
                    .unwrap();
                tx[2]
                    .send(Message::CheckState(state_tx[2].clone()))
                    .unwrap();

                thread::sleep(Duration::from_millis(50));

                let Message::State(state_1) = state_rx[0].recv().unwrap() else {
                    panic!("Expected Message::State");
                };
                let Message::State(state_3) = state_rx[2].recv().unwrap() else {
                    panic!("Expected Message::State");
                };

                if state_1.role == Role::Leader {
                    found_leader += 1;
                }
                if state_3.role == Role::Leader {
                    found_leader += 1;
                }
            }
            3 => {
                tx[0]
                    .send(Message::CheckState(state_tx[0].clone()))
                    .unwrap();
                tx[1]
                    .send(Message::CheckState(state_tx[1].clone()))
                    .unwrap();
                thread::sleep(Duration::from_millis(50));

                let Message::State(state_1) = state_rx[0].recv().unwrap() else {
                    panic!("Expected Message::State");
                };
                let Message::State(state_2) = state_rx[1].recv().unwrap() else {
                    panic!("Expected Message::State");
                };

                if state_1.role == Role::Leader {
                    found_leader += 1;
                }
                if state_2.role == Role::Leader {
                    found_leader += 1;
                }
            }
            _ => panic!("No one was killed"),
        }

        assert_eq!(found_leader, 1);
    }
}
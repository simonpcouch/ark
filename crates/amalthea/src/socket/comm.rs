/*
 * comm.rs
 *
 * Copyright (C) 2023 Posit Software, PBC. All rights reserved.
 *
 */

use crossbeam::channel::Receiver;
use crossbeam::channel::Sender;
use dyn_clone::DynClone;

use crate::comm::comm_channel::CommMsg;

/**
 * A `CommSocket` is a relay between the back end and the front end of a comm.
 * It stores the comm's metadata and handles sending and receiving messages.
 *
 * The socket is a bi-directional channel between the front end and the back
 * end. The terms `incoming` and `outgoing` here refer to the direction of the
 * message flow; that is, `incoming` messages are messages that are received
 * from the front end, and `outgoing` messages are messages that are sent to the
 * front end.
 */
#[derive(Clone)]
pub struct CommSocket {
    /// The comm's unique identifier.
    pub comm_id: String,

    /// The comm's name. This is a freeform string, but it's typically a member
    /// of the Comm enum.
    pub comm_name: String,

    /// The identity of the comm's initiator. This is used to determine whether
    /// the comm is owned by the front end or the back end.
    pub initiator: CommInitiator,

    /// The channel receiving messages from the back end that are to be relayed
    /// to the front end (ultimately via IOPub). These messages are freeform
    /// JSON values.
    pub outgoing_rx: Receiver<CommMsg>,

    /// The other side of the channel receiving messages from the back end. This
    /// `Sender` is passed to the back end of the comm channel so that it can
    /// send messages to the front end.
    pub outgoing_tx: Sender<CommMsg>,

    /// The channel that will accept messages from the front end and relay them
    /// to the back end.
    pub incoming_tx: Sender<CommMsg>,

    /// The other side of the channel receiving messages from the front end
    pub incoming_rx: Receiver<CommMsg>,

    /// DOCME
    handlers: Option<Box<dyn CommHandling>>,
}

/**
 * Describes the identity of the comm's initiator. This is used to determine
 * whether the comm is owned by the front end or the back end.
 */
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CommInitiator {
    /// The comm was initiated by the front end (user interface).
    FrontEnd,

    /// The comm was initiated by the back end (kernel).
    BackEnd,
}

/**
 * A CommSocket is a relay between the back end and the front end of a comm
 * channel. It stores the comm's metadata and handles sending and receiving
 * messages.
 */
impl CommSocket {
    /**
     * Create a new CommSocket.
     *
     * - `initiator`: The identity of the comm's initiator. This is used to
     *   determine whether the comm is owned by the front end or the back end.
     * - `comm_id`: The comm's unique identifier.
     * - `comm_name`: The comm's name. This is a freeform string since comm
     *    names have no restrictions in the Jupyter protocol, but it's typically a
     *    member of the Comm enum.
     * - `handlers`: DOCME
     */
    pub fn new(
        initiator: CommInitiator,
        comm_id: String,
        comm_name: String,
        handlers: Option<Box<dyn CommHandling>>,
    ) -> Self {
        let (outgoing_tx, outgoing_rx) = crossbeam::channel::unbounded();
        let (incoming_tx, incoming_rx) = crossbeam::channel::unbounded();

        Self {
            comm_id,
            comm_name,
            initiator,
            outgoing_tx,
            outgoing_rx,
            incoming_tx,
            incoming_rx,
            handlers,
        }
    }
}

pub trait CommHandling: DynClone + Send + Sync {
    fn handle_request(&self, message: CommMsg) -> anyhow::Result<bool>;
}

//  We need `Clone` on the `CommSocket` to send it across threads. We use
// the `dyn_clone` crate by dtolnay to help make our trait clonable in the
// dynamic case (e.g. `Box<dyn CommHandling>).
dyn_clone::clone_trait_object!(CommHandling);

/// DOCME
#[derive(Clone)]
pub struct CommHandlers<Evts, Reqs, Reps>
where
    Evts: Clone,
    Reqs: Clone,
    Reps: Clone,
{
    pub request_handler: Option<fn(Reqs) -> anyhow::Result<Reps>>,
    pub event_handler: Option<fn(Evts) -> anyhow::Result<()>>,
}

impl<Evts: Clone, Reqs: Clone, Reps: Clone> CommHandlers<Evts, Reqs, Reps> {
    pub fn new(
        event_handler: Option<fn(Evts) -> anyhow::Result<()>>,
        request_handler: Option<fn(Reqs) -> anyhow::Result<Reps>>,
    ) -> Self {
        Self {
            event_handler,
            request_handler,
        }
    }
}

impl<Evts: Clone, Reqs: Clone, Reps: Clone> CommHandling for CommHandlers<Evts, Reqs, Reps> {
    fn handle_request(&self, message: CommMsg) -> anyhow::Result<bool> {
        let (_id, _data) = if let CommMsg::Rpc(id, data) = message {
            (id, data)
        } else {
            return Ok(false);
        };

        Ok(true)
    }
}

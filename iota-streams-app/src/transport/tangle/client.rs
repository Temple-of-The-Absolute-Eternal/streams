use anyhow::{
    anyhow,
    ensure,
    Result,
};

use core::{
    cmp::Ordering,
    convert::{
        TryFrom,
        TryInto,
    },
};
#[cfg(not(feature = "async"))]
use smol::block_on;

#[cfg(feature = "async")]
use iota_streams_core::prelude::Rc;
#[cfg(feature = "async")]
use core::cell::RefCell;

use iota::{
    client as iota_client,
    Message, MessageId, MessageBuilder, ClientMiner,
    message::payload::{
        indexation::Indexation,
        Payload
    }
};

pub use iota::client::bytes_to_trytes;

use iota_streams_core::{
    prelude::{
        String,
        ToString,
        Vec,
    },
    {Errors::*, wrapped_err, try_or, WrappedError, LOCATION_LOG, Result},
};

use crate::{
    message::BinaryMessage,
    transport::{
        tangle::*,
        *,
    },
};

use futures::future::join_all;
use std::boxed::Box;
use std::str;

#[derive(Clone, Copy)]
pub struct SendTrytesOptions {
    pub depth: u8,
    pub min_weight_magnitude: u8,
    pub local_pow: bool,
    pub threads: usize,
}

impl Default for SendTrytesOptions {
    fn default() -> Self {
        Self {
            depth: 3,
            min_weight_magnitude: 14,
            local_pow: true,
            threads: num_cpus::get(),
        }
    }
}

fn handle_client_result<T>(result: iota_client::Result<T>) -> Result<T> {
    result.map_err(|err| anyhow!("Failed iota_client: {}", err))
}

/// Reconstruct Streams Message from bundle. The input bundle is not checked (for validity of
/// the hash, consistency of indices, etc.). Checked bundles are returned by `bundles_from_trytes`.
pub fn msg_from_tangle_message<F>(message: &Message, link: &TangleAddress) -> Result<TangleMessage<F>> {
    if let Payload::Indexation(i) = message.payload().as_ref().unwrap() {
        let binary = BinaryMessage::new(link.clone(), hex::decode(i.data())?.into());
    
        // TODO get timestamp
        let timestamp: u64 = 0;
    
        Ok(TangleMessage { binary, timestamp })
    } else {
        Err(anyhow!("Message is not a Indexation type"))
    }
}

async fn get_messages(client: &iota_client::Client, tx_address: &[u8], tx_tag: &[u8]) -> Result<Vec<Message>> {
    let msg_ids = handle_client_result(client.get_message()
            .index(&hex::encode([tx_address, tx_tag].concat()))
            .await
        ).unwrap();
    ensure!(!msg_ids.is_empty(), "Messade ids not found.");

    let msgs = join_all(
        msg_ids.iter().map(|msg| {
            async move {
                handle_client_result(client
                    .get_message()
                    .data(msg)
                    .await
                ).unwrap()
            }
        }
    )).await;
    ensure!(!msgs.is_empty(), "Messages not found.");
    Ok(msgs)
}

fn make_bundle(
    address: &[u8],
    tag: &[u8],
    body: &[u8],
    _timestamp: u64,
    trunk: MessageId,
    branch: MessageId,
) -> Result<Vec<Message>> {
    let mut msgs = Vec::new();

    dbg!( hex::encode([address, tag].concat()));
    let payload = Indexation::new(
        hex::encode([address, tag].concat()), 
        body).unwrap();
    //TODO: Multiple messages if payload size is over max. Currently no max decided
    let msg = MessageBuilder::<ClientMiner>::new()
        .with_parent1(trunk)
        .with_parent2(branch)
        .with_payload(Payload::Indexation(Box::new(payload)))
        .finish();

    msgs.push(msg.unwrap());
    Ok(msgs)
}

pub fn msg_to_tangle<F>(
    msg: &BinaryMessage<F, TangleAddress>,
    timestamp: u64,
    trunk: MessageId,
    branch: MessageId,
) -> Result<Vec<Message>> {
    make_bundle(
        msg.link.appinst.as_ref(),
        msg.link.msgid.as_ref(),
        &msg.body.bytes,
        timestamp,
        trunk,
        branch,
    )
}

async fn send_messages(client: &iota_client::Client, _opt: &SendTrytesOptions, msgs: Vec<Message>) -> Result<Vec<MessageId>> {
    let msgs = join_all(
        msgs.iter().map(|msg| {
            async move {
                handle_client_result(client.post_message(msg).await).unwrap()
            }
        }
    )).await;

    Ok(msgs)
}

#[derive(Clone, Copy)]
pub struct SendTrytesOptions {
    pub depth: u8,
    pub min_weight_magnitude: u8,
    pub local_pow: bool,
    pub threads: usize,
}

#[cfg(feature = "num_cpus")]
fn get_num_cpus() -> usize {
    num_cpus::get()
}

#[cfg(not(feature = "num_cpus"))]
fn get_num_cpus() -> usize {
    1_usize
}

impl Default for SendTrytesOptions {
    fn default() -> Self {
        Self {
            depth: 3,
            min_weight_magnitude: 14,
            local_pow: true,
            threads: get_num_cpus(),
        }
    }
}

fn handle_client_result<T>(result: iota_client::Result<T>) -> Result<T> {
    result.map_err(|err| wrapped_err!(ClientOperationFailure, WrappedError(err)))
}

async fn get_bundles(client: &iota_client::Client, tx_address: Address, tx_tag: Tag) -> Result<Vec<Transaction>> {
    let find_bundles = handle_client_result(
        client.find_transactions()
            .tags(&vec![tx_tag][..])
            .addresses(&vec![tx_address][..])
            .send()
            .await,
    )?;
    try_or!(!find_bundles.hashes.is_empty(), HashNotFound)?;

    let get_resp = handle_client_result(client.get_trytes(&find_bundles.hashes).await)?;
    try_or!(!get_resp.trytes.is_empty(), TransactionContentsNotFound)?;
    Ok(get_resp.trytes)
}

async fn send_trytes(client: &iota_client::Client, opt: &SendTrytesOptions, txs: Vec<Transaction>) -> Result<Vec<Transaction>> {
    let attached_txs = handle_client_result(
        client.send_trytes()
            .min_weight_magnitude(opt.min_weight_magnitude)
            .depth(opt.depth)
            .trytes(txs)
            .send()
            .await,
    )?;
    Ok(attached_txs)
}

pub async fn async_send_message_with_options<F>(client: &iota_client::Client, msg: &TangleMessage<F>, opt: &SendTrytesOptions) -> Result<()> {
    // TODO: Get trunk and branch hashes. Although, `send_trytes` should get these hashes.
    let tips = client.get_tips().await.unwrap();
    let messages = msg_to_tangle(&msg.binary, msg.timestamp, tips.0, tips.1)?;

    // Ignore attached transactions.
    send_messages(client, opt, messages).await?;
    Ok(())
}

pub async fn async_recv_messages<F>(client: &iota_client::Client, link: &TangleAddress) -> Result<Vec<TangleMessage<F>>> {
    let tx_address = link.appinst.as_ref();
    let tx_tag = link.msgid.as_ref();
    match get_messages(client, tx_address, tx_tag).await {
        Ok(txs) => Ok(txs.iter()
            .map(|b| msg_from_tangle_message(b, link).unwrap())
            .collect()),
        Err(_) => Ok(Vec::new()), // Just ignore the error?
    }
}

#[cfg(not(feature = "async"))]
pub fn sync_send_message_with_options<F>(client: &iota_client::Client, msg: &TangleMessage<F>, opt: &SendTrytesOptions) -> Result<()> {
    block_on(async_send_message_with_options(client, msg, opt))
}

#[cfg(not(feature = "async"))]
pub fn sync_recv_messages<F>(client: &iota_client::Client, link: &TangleAddress) -> Result<Vec<TangleMessage<F>>> {
    block_on(async_recv_messages(client, link))
}

/// Stub type for iota_client::Client.  Removed: Copy, Default, Clone
pub struct Client {
    send_opt: SendTrytesOptions,
    client: iota_client::Client,
}

impl Default for Client {
    // Creates a new instance which links to a node on localhost:14265
    fn default() -> Self {
        Self {
            send_opt: SendTrytesOptions::default(),
            client: iota_client::ClientBuilder::new().with_node("http://localhost:14265").unwrap().finish().unwrap()
        }
    }
}

impl Client {
    // Create an instance of Client with a ready client and its send options
    pub fn new(options: SendTrytesOptions, client: iota_client::Client) -> Self {
        Self {
            send_opt: options,
            client: client
        }
    }

    // Create an instance of Client with a node pointing to the given URL
    pub fn new_from_url(url: &str) -> Self {
        Self {
            send_opt: SendTrytesOptions::default(),
            client: iota_client::ClientBuilder::new().with_node(url).unwrap().finish().unwrap()
        }
    }

    pub fn add_node(&mut self, url: &str) -> Result<bool> {
        self.client.add_node(url).map_err(|e|
            wrapped_err!(ClientOperationFailure, WrappedError(e))
        )
    }
}

impl TransportOptions for Client {
    type SendOptions = SendTrytesOptions;
    fn get_send_options(&self) -> SendTrytesOptions {
        self.send_opt.clone()
    }
    fn set_send_options(&mut self, opt: SendTrytesOptions) {
        self.send_opt = opt;
    }

    type RecvOptions = ();
    fn get_recv_options(&self) -> () {}
    fn set_recv_options(&mut self, _opt: ()) {}
}

#[cfg(not(feature = "async"))]
impl<F> Transport<TangleAddress, TangleMessage<F>> for Client {
    /// Send a Streams message over the Tangle with the current timestamp and default SendTrytesOptions.
    fn send_message(&mut self, msg: &TangleMessage<F>) -> Result<()> {
        sync_send_message_with_options(&self.client, msg, &self.send_opt)
    }

    /// Receive a message.
    fn recv_messages(&mut self, link: &TangleAddress) -> Result<Vec<TangleMessage<F>>> {
        sync_recv_messages(&self.client, link)
    }
}

#[cfg(feature = "async")]
#[async_trait(?Send)]
impl<F> Transport<TangleAddress, TangleMessage<F>> for Client
where
    F: 'static + core::marker::Send + core::marker::Sync,
{
    /// Send a Streams message over the Tangle with the current timestamp and default SendTrytesOptions.
    async fn send_message(&mut self, msg: &TangleMessage<F>) -> Result<()> {
        async_send_message_with_options(&self.client, msg, &self.send_opt).await
    }

    /// Receive a message.
    async fn recv_messages(&mut self, link: &TangleAddress) -> Result<Vec<TangleMessage<F>>> {
        async_recv_messages(&self.client, link).await
    }

    async fn recv_message(&mut self, link: &TangleAddress) -> Result<TangleMessage<F>> {
        let mut msgs = self.recv_messages(link).await?;
        if let Some(msg) = msgs.pop() {
            try_or!(msgs.is_empty(), MessageNotUnique(link.to_string()))?;
            Ok(msg)
        } else {
            err!(MessageLinkNotFound(link.to_string()))
        }
    }
}

// It's safe to impl async trait for Rc<RefCell<T>> targeting wasm as it's single-threaded.
#[cfg(feature = "async")]
#[async_trait(?Send)]
impl<F> Transport<TangleAddress, TangleMessage<F>> for Rc<RefCell<Client>>
where
    F: 'static + core::marker::Send + core::marker::Sync,
{
    /// Send a Streams message over the Tangle with the current timestamp and default SendTrytesOptions.
    async fn send_message(&mut self, msg: &TangleMessage<F>) -> Result<()> {
        match (&*self).try_borrow_mut() {
            Ok(mut tsp) => async_send_message_with_options(&tsp.client, msg, &tsp.send_opt).await,
            Err(_err) => err!(TransportNotAvailable),
        }
    }

    /// Receive a message.
    async fn recv_messages(&mut self, link: &TangleAddress) -> Result<Vec<TangleMessage<F>>> {
        match (&*self).try_borrow_mut() {
            Ok(mut tsp) => async_recv_messages(&tsp.client, link).await,
            Err(err) => err!(TransportNotAvailable),
        }
    }

    async fn recv_message(&mut self, link: &TangleAddress) -> Result<TangleMessage<F>> {
        match (&*self).try_borrow_mut() {
            Ok(mut tsp) => {
                let mut msgs = async_recv_messages(&tsp.client, link).await?;
                if let Some(msg) = msgs.pop() {
                    try_or!(msgs.is_empty(), MessageNotUnique(link.msgid.to_string()));
                    Ok(msg)
                } else {
                    err!(MessageLinkNotFound(link.msgid.to_string()))
                }
            },
            Err(err) => err!(TransportNotAvailable),
        }
    }
}

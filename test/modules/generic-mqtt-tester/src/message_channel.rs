use async_trait::async_trait;
use futures_util::{
    future::{self, Either},
    stream::StreamExt,
};
use mpsc::{Receiver, UnboundedReceiver, UnboundedSender};
use tokio::sync::mpsc;
use tracing::{error, info};

use mqtt3::{
    proto::{Publication, QoS},
    PublishHandle, ReceivedPublication,
};
use trc_client::{MessageTestResult, TrcClient};

use crate::{parse_sequence_number, MessageTesterError, ShutdownHandle, RECEIVE_SOURCE};

/// Responsible for receiving publications and taking some action.
#[async_trait]
pub trait MessageHandler {
    /// Starts handling messages sent to the handler
    async fn handle(&mut self, publication: ReceivedPublication) -> Result<(), MessageTesterError>;
}

/// Responsible for receiving publications and reporting result to the Test Result Coordinator.
pub struct ReportResultMessageHandler {
    reporting_client: TrcClient,
    tracking_id: String,
    batch_id: String,
}

impl ReportResultMessageHandler {
    pub fn new(reporting_client: TrcClient, tracking_id: String, batch_id: String) -> Self {
        Self {
            reporting_client,
            tracking_id,
            batch_id,
        }
    }
}

#[async_trait]
impl MessageHandler for ReportResultMessageHandler {
    async fn handle(
        &mut self,
        received_publication: ReceivedPublication,
    ) -> Result<(), MessageTesterError> {
        let sequence_number = parse_sequence_number(&received_publication);

        info!(
            "reporting result for publication with sequence number {}",
            sequence_number,
        );
        let result = MessageTestResult::new(
            self.tracking_id.clone(),
            self.batch_id.clone(),
            sequence_number,
        );

        let test_type = trc_client::TestType::Messages;
        let created_at = chrono::Utc::now();
        self.reporting_client
            .report_result(RECEIVE_SOURCE.to_string(), result, test_type, created_at)
            .await
            .map_err(MessageTesterError::ReportResult)?;

        Ok(())
    }
}

/// Responsible for receiving publications and sending them back to the downstream edge.
pub struct RelayingMessageHandler {
    publish_handle: PublishHandle,
    topic: String,
}

impl RelayingMessageHandler {
    pub fn new(publish_handle: PublishHandle, topic: String) -> Self {
        Self {
            publish_handle,
            topic,
        }
    }
}

#[async_trait]
impl MessageHandler for RelayingMessageHandler {
    async fn handle(
        &mut self,
        received_publication: ReceivedPublication,
    ) -> Result<(), MessageTesterError> {
        let sequence_number = parse_sequence_number(&received_publication);

        info!(
            "relaying publication with sequence number {}",
            sequence_number,
        );
        let new_publication = Publication {
            topic_name: self.topic.clone(),
            qos: QoS::ExactlyOnce,
            retain: true,
            payload: received_publication.payload,
        };
        self.publish_handle
            .publish(new_publication)
            .await
            .map_err(MessageTesterError::Publish)?;

        Ok(())
    }
}

/// Serves as a channel between a mqtt client's received publications and a message handler.
/// Exposes a message channel to send messages to a separately running thread that will listen
/// for incoming messages and handle them according to custom message handler.
pub struct MessageChannel<H: ?Sized + MessageHandler + Send> {
    publication_sender: UnboundedSender<ReceivedPublication>,
    publication_receiver: UnboundedReceiver<ReceivedPublication>,
    shutdown_handle: ShutdownHandle,
    shutdown_recv: Receiver<()>,
    message_handler: Box<H>,
}

impl<H> MessageChannel<H>
where
    H: MessageHandler + ?Sized + Send,
{
    pub fn new(message_handler: Box<H>) -> Self {
        let (publication_sender, publication_receiver) =
            mpsc::unbounded_channel::<ReceivedPublication>();
        let (shutdown_send, shutdown_recv) = mpsc::channel::<()>(1);
        let shutdown_handle = ShutdownHandle::new(shutdown_send);

        Self {
            publication_sender,
            publication_receiver,
            shutdown_handle,
            shutdown_recv,
            message_handler,
        }
    }

    pub async fn run(mut self) -> Result<(), MessageTesterError> {
        info!("starting message channel");
        loop {
            let received_pub = self.publication_receiver.next();
            let shutdown_signal = self.shutdown_recv.next();

            match future::select(received_pub, shutdown_signal).await {
                Either::Left((received_publication, _)) => {
                    if let Some(received_publication) = received_publication {
                        self.message_handler.handle(received_publication).await?;
                    } else {
                        error!("failed listening for incoming publication");
                        return Err(MessageTesterError::ListenForIncomingPublications);
                    }
                }
                Either::Right((shutdown_signal, _)) => {
                    if shutdown_signal.is_some() {
                        info!("received shutdown signal");
                        return Ok(());
                    } else {
                        error!("failed listening for shutdown");
                        return Err(MessageTesterError::ListenForShutdown);
                    }
                }
            };
        }
    }

    pub fn message_channel(&self) -> UnboundedSender<ReceivedPublication> {
        self.publication_sender.clone()
    }

    pub fn shutdown_handle(&self) -> ShutdownHandle {
        self.shutdown_handle.clone()
    }
}
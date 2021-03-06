// Copyright 2020 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{
    cmp::min,
    convert::TryInto,
    fmt,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use matrix_sdk_common::{
    events::{
        room::{encrypted::EncryptedEventContent, encryption::EncryptionEventContent},
        AnyMessageEventContent, AnySyncRoomEvent, EventContent, SyncMessageEvent,
    },
    identifiers::{DeviceId, EventEncryptionAlgorithm, RoomId},
    instant::Instant,
    locks::Mutex,
    Raw,
};
use olm_rs::{
    errors::OlmGroupSessionError, inbound_group_session::OlmInboundGroupSession,
    outbound_group_session::OlmOutboundGroupSession, PicklingMode,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use zeroize::Zeroize;

pub use olm_rs::{
    account::IdentityKeys,
    session::{OlmMessage, PreKeyMessage},
    utility::OlmUtility,
};

use crate::error::{EventError, MegolmResult};

const ROTATION_PERIOD: Duration = Duration::from_millis(604800000);
const ROTATION_MESSAGES: u64 = 100;

/// Settings for an encrypted room.
///
/// This determines the algorithm and rotation periods of a group session.
#[derive(Debug)]
pub struct EncryptionSettings {
    /// The encryption algorithm that should be used in the room.
    pub algorithm: EventEncryptionAlgorithm,
    /// How long the session should be used before changing it.
    pub rotation_period: Duration,
    /// How many messages should be sent before changing the session.
    pub rotation_period_msgs: u64,
}

impl Default for EncryptionSettings {
    fn default() -> Self {
        Self {
            algorithm: EventEncryptionAlgorithm::MegolmV1AesSha2,
            rotation_period: ROTATION_PERIOD,
            rotation_period_msgs: ROTATION_MESSAGES,
        }
    }
}

impl From<&EncryptionEventContent> for EncryptionSettings {
    fn from(content: &EncryptionEventContent) -> Self {
        let rotation_period: Duration = content
            .rotation_period_ms
            .map_or(ROTATION_PERIOD, |r| Duration::from_millis(r.into()));
        let rotation_period_msgs: u64 = content
            .rotation_period_msgs
            .map_or(ROTATION_MESSAGES, Into::into);

        Self {
            algorithm: content.algorithm.clone(),
            rotation_period,
            rotation_period_msgs,
        }
    }
}

/// The private session key of a group session.
/// Can be used to create a new inbound group session.
#[derive(Clone, Debug, Serialize, Zeroize)]
#[zeroize(drop)]
pub struct GroupSessionKey(pub String);

/// Inbound group session.
///
/// Inbound group sessions are used to exchange room messages between a group of
/// participants. Inbound group sessions are used to decrypt the room messages.
#[derive(Clone)]
pub struct InboundGroupSession {
    inner: Arc<Mutex<OlmInboundGroupSession>>,
    session_id: Arc<String>,
    pub(crate) sender_key: Arc<String>,
    pub(crate) signing_key: Arc<String>,
    pub(crate) room_id: Arc<RoomId>,
    forwarding_chains: Arc<Mutex<Option<Vec<String>>>>,
}

impl InboundGroupSession {
    /// Create a new inbound group session for the given room.
    ///
    /// These sessions are used to decrypt room messages.
    ///
    /// # Arguments
    ///
    /// * `sender_key` - The public curve25519 key of the account that
    /// sent us the session
    ///
    /// * `signing_key` - The public ed25519 key of the account that
    /// sent us the session.
    ///
    /// * `room_id` - The id of the room that the session is used in.
    ///
    /// * `session_key` - The private session key that is used to decrypt
    /// messages.
    pub fn new(
        sender_key: &str,
        signing_key: &str,
        room_id: &RoomId,
        session_key: GroupSessionKey,
    ) -> Result<Self, OlmGroupSessionError> {
        let session = OlmInboundGroupSession::new(&session_key.0)?;
        let session_id = session.session_id();

        Ok(InboundGroupSession {
            inner: Arc::new(Mutex::new(session)),
            session_id: Arc::new(session_id),
            sender_key: Arc::new(sender_key.to_owned()),
            signing_key: Arc::new(signing_key.to_owned()),
            room_id: Arc::new(room_id.clone()),
            forwarding_chains: Arc::new(Mutex::new(None)),
        })
    }

    /// Store the group session as a base64 encoded string.
    ///
    /// # Arguments
    ///
    /// * `pickle_mode` - The mode that was used to pickle the group session,
    /// either an unencrypted mode or an encrypted using passphrase.
    pub async fn pickle(&self, pickle_mode: PicklingMode) -> PickledInboundGroupSession {
        let pickle = self.inner.lock().await.pickle(pickle_mode);

        PickledInboundGroupSession {
            pickle: InboundGroupSessionPickle::from(pickle),
            sender_key: self.sender_key.to_string(),
            signing_key: self.signing_key.to_string(),
            room_id: (&*self.room_id).clone(),
            forwarding_chains: self.forwarding_chains.lock().await.clone(),
        }
    }

    /// Restore a Session from a previously pickled string.
    ///
    /// Returns the restored group session or a `OlmGroupSessionError` if there
    /// was an error.
    ///
    /// # Arguments
    ///
    /// * `pickle` - The pickled version of the `InboundGroupSession`.
    ///
    /// * `pickle_mode` - The mode that was used to pickle the session, either
    /// an unencrypted mode or an encrypted using passphrase.
    pub fn from_pickle(
        pickle: PickledInboundGroupSession,
        pickle_mode: PicklingMode,
    ) -> Result<Self, OlmGroupSessionError> {
        let session = OlmInboundGroupSession::unpickle(pickle.pickle.0, pickle_mode)?;
        let session_id = session.session_id();

        Ok(InboundGroupSession {
            inner: Arc::new(Mutex::new(session)),
            session_id: Arc::new(session_id),
            sender_key: Arc::new(pickle.sender_key),
            signing_key: Arc::new(pickle.signing_key),
            room_id: Arc::new(pickle.room_id),
            forwarding_chains: Arc::new(Mutex::new(pickle.forwarding_chains)),
        })
    }

    /// Returns the unique identifier for this session.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Get the first message index we know how to decrypt.
    pub async fn first_known_index(&self) -> u32 {
        self.inner.lock().await.first_known_index()
    }

    /// Decrypt the given ciphertext.
    ///
    /// Returns the decrypted plaintext or an `OlmGroupSessionError` if
    /// decryption failed.
    ///
    /// # Arguments
    ///
    /// * `message` - The message that should be decrypted.
    pub async fn decrypt_helper(
        &self,
        message: String,
    ) -> Result<(String, u32), OlmGroupSessionError> {
        self.inner.lock().await.decrypt(message)
    }

    /// Decrypt an event from a room timeline.
    ///
    /// # Arguments
    ///
    /// * `event` - The event that should be decrypted.
    pub async fn decrypt(
        &self,
        event: &SyncMessageEvent<EncryptedEventContent>,
    ) -> MegolmResult<(Raw<AnySyncRoomEvent>, u32)> {
        let content = match &event.content {
            EncryptedEventContent::MegolmV1AesSha2(c) => c,
            _ => return Err(EventError::UnsupportedAlgorithm.into()),
        };

        let (plaintext, message_index) = self.decrypt_helper(content.ciphertext.clone()).await?;

        let mut decrypted_value = serde_json::from_str::<Value>(&plaintext)?;
        let decrypted_object = decrypted_value
            .as_object_mut()
            .ok_or(EventError::NotAnObject)?;

        // TODO better number conversion here.
        let server_ts = event
            .origin_server_ts
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let server_ts: i64 = server_ts.try_into().unwrap_or_default();

        decrypted_object.insert("sender".to_owned(), event.sender.to_string().into());
        decrypted_object.insert("event_id".to_owned(), event.event_id.to_string().into());
        decrypted_object.insert("origin_server_ts".to_owned(), server_ts.into());

        decrypted_object.insert(
            "unsigned".to_owned(),
            serde_json::to_value(&event.unsigned).unwrap_or_default(),
        );

        Ok((
            serde_json::from_value::<Raw<AnySyncRoomEvent>>(decrypted_value)?,
            message_index,
        ))
    }
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for InboundGroupSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InboundGroupSession")
            .field("session_id", &self.session_id())
            .finish()
    }
}

impl PartialEq for InboundGroupSession {
    fn eq(&self, other: &Self) -> bool {
        self.session_id() == other.session_id()
    }
}

/// A pickled version of an `InboundGroupSession`.
///
/// Holds all the information that needs to be stored in a database to restore
/// an InboundGroupSession.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PickledInboundGroupSession {
    /// The pickle string holding the InboundGroupSession.
    pub pickle: InboundGroupSessionPickle,
    /// The public curve25519 key of the account that sent us the session
    pub sender_key: String,
    /// The public ed25519 key of the account that sent us the session.
    pub signing_key: String,
    /// The id of the room that the session is used in.
    pub room_id: RoomId,
    /// The list of claimed ed25519 that forwarded us this key. Will be None if
    /// we dirrectly received this session.
    pub forwarding_chains: Option<Vec<String>>,
}

/// The typed representation of a base64 encoded string of the GroupSession pickle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundGroupSessionPickle(String);

impl From<String> for InboundGroupSessionPickle {
    fn from(pickle_string: String) -> Self {
        InboundGroupSessionPickle(pickle_string)
    }
}

impl InboundGroupSessionPickle {
    /// Get the string representation of the pickle.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Outbound group session.
///
/// Outbound group sessions are used to exchange room messages between a group
/// of participants. Outbound group sessions are used to encrypt the room
/// messages.
#[derive(Clone)]
pub struct OutboundGroupSession {
    inner: Arc<Mutex<OlmOutboundGroupSession>>,
    device_id: Arc<Box<DeviceId>>,
    account_identity_keys: Arc<IdentityKeys>,
    session_id: Arc<String>,
    room_id: Arc<RoomId>,
    creation_time: Arc<Instant>,
    message_count: Arc<AtomicU64>,
    shared: Arc<AtomicBool>,
    settings: Arc<EncryptionSettings>,
}

impl OutboundGroupSession {
    /// Create a new outbound group session for the given room.
    ///
    /// Outbound group sessions are used to encrypt room messages.
    ///
    /// # Arguments
    ///
    /// * `device_id` - The id of the device that created this session.
    ///
    /// * `identity_keys` - The identity keys of the account that created this
    /// session.
    ///
    /// * `room_id` - The id of the room that the session is used in.
    ///
    /// * `settings` - Settings determining the algorithm and rotation period of
    /// the outbound group session.
    pub fn new(
        device_id: Arc<Box<DeviceId>>,
        identity_keys: Arc<IdentityKeys>,
        room_id: &RoomId,
        settings: EncryptionSettings,
    ) -> Self {
        let session = OlmOutboundGroupSession::new();
        let session_id = session.session_id();

        OutboundGroupSession {
            inner: Arc::new(Mutex::new(session)),
            room_id: Arc::new(room_id.to_owned()),
            device_id,
            account_identity_keys: identity_keys,
            session_id: Arc::new(session_id),
            creation_time: Arc::new(Instant::now()),
            message_count: Arc::new(AtomicU64::new(0)),
            shared: Arc::new(AtomicBool::new(false)),
            settings: Arc::new(settings),
        }
    }

    /// Encrypt the given plaintext using this session.
    ///
    /// Returns the encrypted ciphertext.
    ///
    /// # Arguments
    ///
    /// * `plaintext` - The plaintext that should be encrypted.
    pub(crate) async fn encrypt_helper(&self, plaintext: String) -> String {
        let session = self.inner.lock().await;
        self.message_count.fetch_add(1, Ordering::SeqCst);
        session.encrypt(plaintext)
    }

    /// Encrypt a room message for the given room.
    ///
    /// Beware that a group session needs to be shared before this method can be
    /// called using the `share_group_session()` method.
    ///
    /// Since group sessions can expire or become invalid if the room membership
    /// changes client authors should check with the
    /// `should_share_group_session()` method if a new group session needs to
    /// be shared.
    ///
    /// # Arguments
    ///
    /// * `content` - The plaintext content of the message that should be
    /// encrypted.
    ///
    /// # Panics
    ///
    /// Panics if the content can't be serialized.
    pub async fn encrypt(&self, content: AnyMessageEventContent) -> EncryptedEventContent {
        let json_content = json!({
            "content": content,
            "room_id": &*self.room_id,
            "type": content.event_type(),
        });

        let plaintext = cjson::to_string(&json_content).unwrap_or_else(|_| {
            panic!(format!(
                "Can't serialize {} to canonical JSON",
                json_content
            ))
        });

        let ciphertext = self.encrypt_helper(plaintext).await;

        EncryptedEventContent::MegolmV1AesSha2(
            matrix_sdk_common::events::room::encrypted::MegolmV1AesSha2ContentInit {
                ciphertext,
                sender_key: self.account_identity_keys.curve25519().to_owned(),
                session_id: self.session_id().to_owned(),
                device_id: (&*self.device_id).to_owned(),
            }
            .into(),
        )
    }

    /// Check if the session has expired and if it should be rotated.
    ///
    /// A session will expire after some time or if enough messages have been
    /// encrypted using it.
    pub fn expired(&self) -> bool {
        let count = self.message_count.load(Ordering::SeqCst);

        count >= self.settings.rotation_period_msgs
            || self.creation_time.elapsed()
                // Since the encryption settings are provided by users and not
                // checked someone could set a really low rotation perdiod so
                // clamp it at a minute.
                >= min(self.settings.rotation_period, Duration::from_secs(3600))
    }

    /// Mark the session as shared.
    ///
    /// Messages shouldn't be encrypted with the session before it has been
    /// shared.
    pub fn mark_as_shared(&self) {
        self.shared.store(true, Ordering::Relaxed);
    }

    /// Check if the session has been marked as shared.
    pub fn shared(&self) -> bool {
        self.shared.load(Ordering::Relaxed)
    }

    /// Get the session key of this session.
    ///
    /// A session key can be used to to create an `InboundGroupSession`.
    pub async fn session_key(&self) -> GroupSessionKey {
        let session = self.inner.lock().await;
        GroupSessionKey(session.session_key())
    }

    /// Returns the unique identifier for this session.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Get the current message index for this session.
    ///
    /// Each message is sent with an increasing index. This returns the
    /// message index that will be used for the next encrypted message.
    pub async fn message_index(&self) -> u32 {
        let session = self.inner.lock().await;
        session.session_message_index()
    }

    /// Get the outbound group session key as a json value that can be sent as a
    /// m.room_key.
    pub async fn as_json(&self) -> Value {
        json!({
            "algorithm": EventEncryptionAlgorithm::MegolmV1AesSha2,
            "room_id": &*self.room_id,
            "session_id": &*self.session_id,
            "session_key": self.session_key().await,
            "chain_index": self.message_index().await,
        })
    }
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for OutboundGroupSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutboundGroupSession")
            .field("session_id", &self.session_id)
            .field("room_id", &self.room_id)
            .field("creation_time", &self.creation_time)
            .field("message_count", &self.message_count)
            .finish()
    }
}

#[cfg(test)]
mod test {
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };

    use matrix_sdk_common::{
        events::{
            room::message::{MessageEventContent, TextMessageEventContent},
            AnyMessageEventContent,
        },
        identifiers::{room_id, user_id},
    };

    use super::EncryptionSettings;
    use crate::Account;

    #[tokio::test]
    #[cfg(not(target_os = "macos"))]
    async fn expiration() {
        let settings = EncryptionSettings {
            rotation_period_msgs: 1,
            ..Default::default()
        };

        let account = Account::new(&user_id!("@alice:example.org"), "DEVICEID".into());
        let (session, _) = account
            .create_group_session_pair(&room_id!("!test_room:example.org"), settings)
            .await
            .unwrap();

        assert!(!session.expired());
        let _ = session
            .encrypt(AnyMessageEventContent::RoomMessage(
                MessageEventContent::Text(TextMessageEventContent::plain("Test message")),
            ))
            .await;
        assert!(session.expired());

        let settings = EncryptionSettings {
            rotation_period: Duration::from_millis(100),
            ..Default::default()
        };

        let (mut session, _) = account
            .create_group_session_pair(&room_id!("!test_room:example.org"), settings)
            .await
            .unwrap();

        assert!(!session.expired());
        session.creation_time = Arc::new(Instant::now() - Duration::from_secs(60 * 60));
        assert!(session.expired());
    }
}

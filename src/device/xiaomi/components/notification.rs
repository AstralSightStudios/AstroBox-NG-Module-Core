use anyhow::{Result, ensure};
use pb::xiaomi::protocol::{self, notify_data};
use prost::Message;
use sha2::{Digest, Sha256};

use crate::{
    device::xiaomi::{XiaomiDevice, packet::cipher},
    ecs::Component,
};

#[derive(Clone, Debug)]
pub enum LiveActivity {
    Focus(notify_data::Focus),
    FocusV2(notify_data::FocusV2),
}

#[derive(Clone, Debug)]
pub struct NotificationContent {
    pub id: u32,
    pub app_name: String,
    pub title: String,
    pub sub_title: String,
    pub text: String,
    pub timestamp_ms: u64,
    pub live_activity: Option<LiveActivity>,
}

#[derive(Component, Default)]
pub struct NotificationSystem;

impl NotificationSystem {
    /// Success means queued for transport, not acknowledged or displayed by the device.
    pub fn send(
        &self,
        device: &mut XiaomiDevice,
        source: &str,
        content: NotificationContent,
    ) -> Result<()> {
        let packet = build_notification(source, content)?;
        cipher::enqueue_pb_packet(device, packet, "NotificationSystem::send");
        Ok(())
    }

    pub fn remove(&self, device: &mut XiaomiDevice, source: &str, id: u32) -> Result<()> {
        let packet = build_removal(source, id)?;
        cipher::enqueue_pb_packet(device, packet, "NotificationSystem::remove");
        Ok(())
    }
}

fn notification_id(source: &str, id: u32) -> Result<protocol::NotifyId> {
    ensure!(!source.trim().is_empty(), "notification source is empty");
    let namespace = hex::encode(Sha256::digest(source.as_bytes()));
    let key = format!("{namespace}:{id}");
    let hash = Sha256::digest(key.as_bytes());
    Ok(protocol::NotifyId {
        uid: u32::from_le_bytes(hash[..4].try_into().unwrap()),
        app_id: format!("moe.astralsight.astrobox.plugin.{namespace}"),
        app_group: namespace,
        key,
    })
}

fn packet(
    id: protocol::notification::NotificationId,
    payload: protocol::notification::Payload,
) -> protocol::WearPacket {
    protocol::WearPacket {
        r#type: protocol::wear_packet::Type::Notification as i32,
        id: id as u32,
        payload: Some(protocol::wear_packet::Payload::Notification(
            protocol::Notification {
                payload: Some(payload),
            },
        )),
    }
}

fn build_notification(source: &str, content: NotificationContent) -> Result<protocol::WearPacket> {
    let id = notification_id(source, content.id)?;
    let (focus, focus_v2) = match content.live_activity {
        Some(LiveActivity::Focus(value)) => (Some(value), None),
        Some(LiveActivity::FocusV2(value)) => (None, Some(value)),
        None => (None, None),
    };
    let result = packet(
        protocol::notification::NotificationId::AddNotify,
        protocol::notification::Payload::Data(protocol::NotifyData {
            app_id: id.app_id,
            app_name: content.app_name,
            title: content.title,
            sub_title: content.sub_title,
            text: content.text,
            date: content.timestamp_ms.to_string(),
            uid: id.uid,
            app_group: id.app_group,
            key: id.key,
            call_type: None,
            call_number: String::new(),
            support_reply: Some(false),
            support_open: Some(false),
            focus,
            focus_v2,
        }),
    );
    ensure!(
        result.encoded_len() <= 16 * 1024,
        "notification exceeds 16 KiB"
    );
    Ok(result)
}

fn build_removal(source: &str, id: u32) -> Result<protocol::WearPacket> {
    Ok(packet(
        protocol::notification::NotificationId::RemoveNotify,
        protocol::notification::Payload::Id(notification_id(source, id)?),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn content() -> NotificationContent {
        NotificationContent {
            id: 7,
            app_name: "Example".into(),
            title: "Title".into(),
            sub_title: String::new(),
            text: "Body".into(),
            timestamp_ms: 1700000000000,
            live_activity: None,
        }
    }

    fn data(packet: &protocol::WearPacket) -> &protocol::NotifyData {
        let Some(protocol::wear_packet::Payload::Notification(notification)) = &packet.payload
        else {
            panic!("notification payload")
        };
        let Some(protocol::notification::Payload::Data(data)) = &notification.payload else {
            panic!("data payload")
        };
        data
    }

    #[test]
    fn send_and_remove_share_identity_and_roundtrip() {
        let sent = build_notification("plugin-a", content()).unwrap();
        assert_eq!(
            sent.r#type,
            protocol::wear_packet::Type::Notification as i32
        );
        assert_eq!(sent.id, 0);
        assert_eq!(
            protocol::WearPacket::decode(sent.encode_to_vec().as_slice()).unwrap(),
            sent
        );
        let value = data(&sent);
        assert_eq!(value.date, "1700000000000");
        assert_eq!(value.support_reply, Some(false));
        assert!(value.focus.is_none() && value.focus_v2.is_none());
        let removed = build_removal("plugin-a", 7).unwrap();
        assert_eq!(removed.id, 1);
        let Some(protocol::wear_packet::Payload::Notification(notification)) = removed.payload
        else {
            panic!()
        };
        let Some(protocol::notification::Payload::Id(id)) = notification.payload else {
            panic!()
        };
        assert_eq!(
            (id.uid, id.app_id, id.app_group, id.key),
            (
                value.uid,
                value.app_id.clone(),
                value.app_group.clone(),
                value.key.clone()
            )
        );
    }

    #[test]
    fn namespaces_are_stable_and_distinct() {
        assert_eq!(
            notification_id("a", 7).unwrap(),
            notification_id("a", 7).unwrap()
        );
        assert_ne!(
            notification_id("a", 7).unwrap(),
            notification_id("b", 7).unwrap()
        );
        assert_ne!(
            notification_id("a", 7).unwrap(),
            notification_id("a", 8).unwrap()
        );
        assert!(notification_id("", 7).is_err());
    }

    #[test]
    fn focus_variants_and_updates_are_preserved() {
        let mut input = content();
        input.live_activity = Some(LiveActivity::Focus(notify_data::Focus {
            style: 1,
            title: notify_data::Text::default(),
            content: notify_data::Text::default(),
            desc: notify_data::Text::default(),
            progress: Some(notify_data::Progress {
                section_count: 4,
                progress: 2,
                color: Some(vec![1, 2, 3]),
            }),
            updatable: Some(true),
            sequence: Some(2),
        }));
        let first = build_notification("a", input.clone()).unwrap();
        assert_eq!(data(&first).focus.as_ref().unwrap().sequence, Some(2));
        assert!(data(&first).focus_v2.is_none());
        input.live_activity = Some(LiveActivity::FocusV2(notify_data::FocusV2 {
            scene: 3,
            ticker: "Next".into(),
            basic_info: notify_data::Info::default(),
            hint_info: None,
            progress: None,
            updatable: Some(true),
            sequence: Some(3),
        }));
        let second = build_notification("a", input).unwrap();
        assert!(data(&second).focus.is_none());
        assert_eq!(data(&second).focus_v2.as_ref().unwrap().sequence, Some(3));
        assert_eq!(data(&first).uid, data(&second).uid);
    }

    #[test]
    fn oversized_notification_is_rejected() {
        let mut input = content();
        input.text = "x".repeat(16 * 1024);
        assert!(build_notification("a", input).is_err());
    }
}

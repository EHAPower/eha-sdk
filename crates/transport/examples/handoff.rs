// Copyright The eha_controller Contributors

//! CAN 与 USB 接收器向应用共同最新缓存交接的纯内存示例。
//!
//! 这只展示既定的交接模型：完整且格式合法的非心跳消息按到达顺序覆盖共同缓存；有效显式
//! 心跳只记录实际入口。它不创建任务、不调度控制，也不代表设备收发或业务执行。

use protocol::{Direction, MAX_MESSAGE_LEN, Message, MessageKind, encode, encode_in_place};
use transport::{can, usb};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Ingress {
    Can,
    Usb,
}

/// 应用层唯一的“最新待处理”缓存。
///
/// 它始终持有一个 `MAX_MESSAGE_LEN` 缓冲。小消息复制到该缓冲；大消息将 CAN 完成 lane
/// 的原缓冲与该缓冲交换。调用方以交换返回的缓冲立刻补回 CAN lane，因此大配置没有第二份
/// 应用层副本，且下一条大小消息仍可继续覆盖这一个缓存。
struct Application<'a> {
    buffer: &'a mut [u8],
    latest: Option<(Ingress, usize)>,
    can_contact: bool,
    usb_contact: bool,
}

impl<'a> Application<'a> {
    fn new(buffer: &'a mut [u8]) -> Self {
        Self {
            buffer,
            latest: None,
            can_contact: false,
            usb_contact: false,
        }
    }

    fn heartbeat(&mut self, ingress: Ingress) {
        match ingress {
            Ingress::Can => self.can_contact = true,
            Ingress::Usb => self.usb_contact = true,
        }
    }

    /// 接纳任意已完成的非心跳短消息；借用在复制完成后立即结束。
    fn accept_copy(&mut self, ingress: Ingress, bytes: &[u8]) -> Result<(), &'static str> {
        if bytes.len() > self.buffer.len() {
            return Err("application cache is too small");
        }
        self.buffer[..bytes.len()].copy_from_slice(bytes);
        self.latest = Some((ingress, bytes.len()));
        Ok(())
    }

    /// 接纳一条大 CAN 消息，返回可补回 CAN lane 的旧应用缓存。
    fn accept_large(
        &mut self,
        ingress: Ingress,
        complete: can::CompletedMessage<'a>,
    ) -> &'a mut [u8] {
        let length = complete.info().length;
        let completed_buffer = complete.into_buffer();
        let replacement = core::mem::replace(&mut self.buffer, completed_buffer);
        self.latest = Some((ingress, length));
        replacement
    }
}

/// 用真实 CAN `Transmitter` 生成帧，再交给真实 `Receiver`；没有手工拼装标识符或帧元数据。
fn deliver_can(
    sender: &mut can::Transmitter<'_>,
    receiver: &mut can::Receiver<'_>,
    lane: can::Lane,
    length: usize,
) -> Result<(), &'static str> {
    sender
        .start_buffer(lane, length, 0)
        .map_err(|_| "CAN sender cannot freeze a valid message")?;
    let mut receiver_completed = false;
    loop {
        let pending = sender
            .next_frame(lane)
            .ok_or("CAN sender did not provide its next frame")?;
        if matches!(
            receiver.receive(&pending.frame, 0, 0),
            can::ReceiveResult::Complete { .. }
        ) {
            receiver_completed = true;
        }
        let event = sender
            .complete(pending.token, can::SubmitResult::Accepted, 0)
            .map_err(|_| "CAN sender rejected its current completion")?;
        if matches!(event, can::SubmitEvent::MessageCommitted { .. }) {
            return receiver_completed
                .then_some(())
                .ok_or("CAN receiver did not complete the submitted message");
        }
    }
}

/// 将 USB sender 产生的每个实际 Bulk 包持续交给 receiver；每次都按 `consumed` 推进，
/// 因而完成一个 COBS 块不会遗失同一包内的剩余字节。
fn deliver_usb_query(
    sender: &mut usb::Sender<'_>,
    receiver: &mut usb::Receiver<'_>,
    length: usize,
) -> Result<(), &'static str> {
    let lane = usb::Lane::Short;
    sender
        .begin_buffer(lane, length, 0)
        .map_err(|_| "USB sender cannot freeze a valid query")?;
    let mut packet = [0; usb::PACKET_LEN];
    let mut receiver_completed = false;
    while let Some(prepared) = sender
        .prepare_packet(lane, &mut packet)
        .map_err(|_| "USB cannot prepare a packet")?
    {
        let mut consumed = 0;
        while consumed < prepared.length {
            let result = receiver.feed(receiver.phase(), &packet[consumed..prepared.length], 0);
            if result.consumed == 0 {
                return Err("USB receiver made no progress");
            }
            consumed += result.consumed;
            receiver_completed |= matches!(result.event, usb::ReceiveEvent::Complete(_));
        }
        if sender.accepted(prepared.token) != usb::SendEvent::PacketAccepted {
            return Err("USB endpoint did not accept the prepared packet");
        }
        let event = sender.completed(prepared.token, 0);
        if matches!(event, usb::SendEvent::MessageCommitted { .. }) {
            return receiver_completed
                .then_some(())
                .ok_or("USB receiver did not complete the submitted query");
        }
    }
    Err("USB sender ended before committing the query")
}

fn main() -> Result<(), &'static str> {
    let mut can_heartbeat = [0; 8];
    let mut can_short = [0; 256];
    let mut can_large = [0; MAX_MESSAGE_LEN];
    let mut can_receiver = can::Receiver::new(
        1,
        Direction::HostToFirmware,
        can::Mode::Classic,
        0,
        can::RxBuffers {
            heartbeat: &mut can_heartbeat,
            short: &mut can_short,
            large: &mut can_large,
        },
    )
    .map_err(|_| "CAN buffers must meet the three lane capacities")?;

    let mut can_tx_heartbeat = [0; 8];
    let mut can_tx_short = [0; 256];
    let mut can_tx_large = [0; MAX_MESSAGE_LEN];
    let mut can_sender = can::Transmitter::new(
        1,
        Direction::HostToFirmware,
        can::Mode::Classic,
        0,
        can::TxBuffers {
            heartbeat: &mut can_tx_heartbeat,
            short: &mut can_tx_short,
            large: &mut can_tx_large,
        },
    )
    .map_err(|_| "CAN sender buffers must meet the three lane capacities")?;

    let mut application_buffer = [0; MAX_MESSAGE_LEN];
    let mut application = Application::new(&mut application_buffer);

    let heartbeat_len = encode(
        Message::Heartbeat,
        can_sender
            .buffer_mut(can::Lane::Heartbeat)
            .map_err(|_| "CAN heartbeat lane is busy")?,
    )
    .map_err(|_| "encode heartbeat")?;
    deliver_can(
        &mut can_sender,
        &mut can_receiver,
        can::Lane::Heartbeat,
        heartbeat_len,
    )?;
    // 心跳从 CAN 完整交入，但从不占用最新业务缓存；只记录它实际抵达的入口。
    if can_receiver.message(can::Lane::Heartbeat).is_none() {
        return Err("CAN heartbeat was not complete");
    }
    application.heartbeat(Ingress::Can);
    can_receiver.discard_completed(can::Lane::Heartbeat);

    let can_query_len = encode(
        Message::Query {
            query_id: 7,
            category: 2,
        },
        can_sender
            .buffer_mut(can::Lane::Short)
            .map_err(|_| "CAN short lane is busy")?,
    )
    .map_err(|_| "encode CAN query")?;
    deliver_can(
        &mut can_sender,
        &mut can_receiver,
        can::Lane::Short,
        can_query_len,
    )?;
    application.accept_copy(
        Ingress::Can,
        can_receiver
            .message(can::Lane::Short)
            .ok_or("CAN query was not complete")?,
    )?;
    can_receiver.discard_completed(can::Lane::Short);

    let mut usb_heartbeat = [0; 8];
    let mut usb_short = [0; 256];
    let mut usb_large = [0; MAX_MESSAGE_LEN];
    let mut usb_receiver = usb::Receiver::new(
        Direction::HostToFirmware,
        &mut usb_heartbeat,
        &mut usb_short,
        &mut usb_large,
    );
    let mut usb_tx_heartbeat = [0; 8];
    let mut usb_tx_short = [0; 256];
    let mut usb_tx_large = [0; MAX_MESSAGE_LEN];
    let mut usb_sender = usb::Sender::new(
        Direction::HostToFirmware,
        &mut usb_tx_heartbeat,
        &mut usb_tx_short,
        &mut usb_tx_large,
    );
    let usb_query_len = encode(
        Message::Query {
            query_id: 8,
            category: 3,
        },
        usb_sender
            .buffer_mut(usb::Lane::Short)
            .ok_or("USB short lane is busy")?,
    )
    .map_err(|_| "encode USB query")?;
    deliver_usb_query(&mut usb_sender, &mut usb_receiver, usb_query_len)?;
    application.accept_copy(
        Ingress::Usb,
        usb_receiver
            .message(usb::Lane::Short)
            .ok_or("USB query was not complete")?,
    )?;
    // 短消息的复制已结束；舍弃完成状态后 USB 可复用原缓冲接收下一条短消息。
    if !usb_receiver.discard_completed(usb::Lane::Short) {
        return Err("USB completed lane cannot be discarded");
    }
    if application.latest != Some((Ingress::Usb, usb_query_len)) {
        return Err("later USB query did not cover the CAN query");
    }

    let first_save_len = {
        let buffer = can_sender
            .buffer_mut(can::Lane::Large)
            .map_err(|_| "CAN large lane is busy")?;
        buffer[4..40].fill(1);
        for (index, byte) in buffer[40..40 + 16_384].iter_mut().enumerate() {
            *byte = index as u8;
        }
        encode_in_place(MessageKind::SaveConfig, 16_420, buffer)
            .map_err(|_| "encode first SaveConfig")?
    };
    deliver_can(
        &mut can_sender,
        &mut can_receiver,
        can::Lane::Large,
        first_save_len,
    )?;
    let first_completed = can_receiver
        .take_completed(can::Lane::Large)
        .ok_or("first SaveConfig was not complete")?;
    // 零复制的大消息交接：返回的旧应用缓存补回 CAN lane，完成的 CAN 缓冲成为唯一最新缓存。
    let replacement = application.accept_large(Ingress::Can, first_completed);
    can_receiver
        .replace_buffer(can::Lane::Large, replacement)
        .map_err(|_| "returned application cache must fit the CAN large lane")?;

    let stop_len = encode(
        Message::Stop,
        can_sender
            .buffer_mut(can::Lane::Short)
            .map_err(|_| "CAN short lane is busy after query")?,
    )
    .map_err(|_| "encode Stop")?;
    deliver_can(
        &mut can_sender,
        &mut can_receiver,
        can::Lane::Short,
        stop_len,
    )?;
    application.accept_copy(
        Ingress::Can,
        can_receiver
            .message(can::Lane::Short)
            .ok_or("Stop was not complete")?,
    )?;
    can_receiver.discard_completed(can::Lane::Short);
    if application.latest != Some((Ingress::Can, stop_len)) {
        return Err("later Stop did not cover SaveConfig");
    }

    let second_save_len = {
        let buffer = can_sender
            .buffer_mut(can::Lane::Large)
            .map_err(|_| "CAN large lane is busy after first SaveConfig")?;
        buffer[4..40].fill(2);
        buffer[40..40 + 16_384].fill(0x5a);
        encode_in_place(MessageKind::SaveConfig, 16_420, buffer)
            .map_err(|_| "encode second SaveConfig")?
    };
    deliver_can(
        &mut can_sender,
        &mut can_receiver,
        can::Lane::Large,
        second_save_len,
    )?;
    let second_completed = can_receiver
        .take_completed(can::Lane::Large)
        .ok_or("second SaveConfig was not complete")?;
    // 同一交换在第二次大配置重复：上一轮接收缓冲已经经由应用缓存回到 CAN lane，可继续复用。
    let replacement = application.accept_large(Ingress::Can, second_completed);
    can_receiver
        .replace_buffer(can::Lane::Large, replacement)
        .map_err(|_| "recycled application cache must fit the CAN large lane")?;

    if application.latest != Some((Ingress::Can, second_save_len)) {
        return Err("second SaveConfig did not cover Stop");
    }
    if !application.can_contact || application.usb_contact {
        return Err("only the actual CAN heartbeat may refresh CAN contact");
    }
    println!(
        "latest non-heartbeat: SaveConfig {second_save_len} B from CAN; CAN contact={}, USB contact={}",
        application.can_contact, application.usb_contact
    );
    Ok(())
}

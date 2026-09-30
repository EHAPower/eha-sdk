// Copyright The eha_controller Contributors

//! 宿主上的实际类型大小与最大记录处理成本；不访问设备，不代表 H723 时序。

use std::{hint::black_box, mem::size_of, time::Instant};

use protocol::{Direction, MAX_MESSAGE_LEN, Message, OperationKey, crc32c, encode, validate};
use transport::{can, usb};

fn median_ns(mut work: impl FnMut()) -> u128 {
    const ITERATIONS: u128 = 100;
    let mut samples = [0; 5];
    for sample in &mut samples {
        let start = Instant::now();
        for _ in 0..ITERATIONS {
            work();
        }
        *sample = start.elapsed().as_nanos() / ITERATIONS;
    }
    samples.sort_unstable();
    samples[2]
}

fn main() {
    println!("实际宿主类型大小（字节），不包含借用的消息存储：");
    for (name, size) in [
        ("Message", size_of::<Message<'_>>()),
        (
            "TelemetryFields",
            size_of::<protocol::responses::TelemetryFields>(),
        ),
        (
            "IdentityFields",
            size_of::<protocol::responses::IdentityFields<'_>>(),
        ),
        (
            "DiagnosticFields",
            size_of::<protocol::responses::DiagnosticFields>(),
        ),
        (
            "ConfigDataFields",
            size_of::<protocol::responses::ConfigDataFields>(),
        ),
        (
            "OperationResultFields",
            size_of::<protocol::responses::OperationResultFields>(),
        ),
        ("CAN Receiver", size_of::<can::Receiver<'_>>()),
        ("CAN Transmitter", size_of::<can::Transmitter<'_>>()),
        ("CAN Frame", size_of::<can::Frame>()),
        ("CAN PendingFrame", size_of::<can::PendingFrame>()),
        ("USB Receiver", size_of::<usb::Receiver<'_>>()),
        ("USB Sender", size_of::<usb::Sender<'_>>()),
        ("USB PreparedPacket", size_of::<usb::PreparedPacket>()),
    ] {
        println!("{name}: {size}");
    }
    println!(
        "每个方向的完整三通道消息存储：{}",
        8 + 256 + MAX_MESSAGE_LEN
    );

    // 明确的两个宿主栈数组；传输层仅处理原始字节，不检查配置字段。
    // 空对象与尾部空白是容量测量向量，不是可提交给设备的配置。
    // 固件应由其资源所有者分配实际存储，不能据本例推定固件栈足够。
    let mut record = [b' '; 16_384];
    record[..2].copy_from_slice(b"{}");
    let mut wire = [0; MAX_MESSAGE_LEN];
    let key_bytes = [1; 36]; // 仅软件向量，不作为设备会话生成方法。
    let key = OperationKey::new(&key_bytes).expect("固定测试键");
    let message = Message::SaveConfig {
        key,
        record: &record,
    };
    let length = encode(message, &mut wire).expect("有界最大记录编码");
    println!("最大 SaveConfig 完整长度：{length}");
    let encode_ns = median_ns(|| {
        black_box(encode(black_box(message), black_box(&mut wire)).expect("合法固定输入"));
    });
    let validate_ns = median_ns(|| {
        black_box(
            validate(black_box(&wire[..length]), Direction::HostToFirmware).expect("完整消息"),
        );
    });
    let crc_ns = median_ns(|| {
        black_box(crc32c(black_box(&wire[..length])));
    });
    println!(
        "宿主 release 每次处理的5组中位数（每组100次）：encode={encode_ns} ns，validate={validate_ns} ns，CRC={crc_ns} ns"
    );
}

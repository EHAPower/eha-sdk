// Copyright The eha-sdk Contributors
//! 通过公开 SDK 留存实板查询、心跳、停止和保存读回证据。
//! 用法：board_check usb SERIAL OUTDIR [save-current|reconnect]
//!       board_check can CONTEXT NODE MODE OUTDIR [PYTHON] [save-current|reconnect]
//! save-current 明确请求以相同容量/业务配置、不同 JSON 空白保存一次，再实际读回。
//! inspect / reconnect-readonly / result:KEYHEX 只读，适用于异常后的取证。
//! 可用 EHA_EXPECT_UID 指定24位十六进制设备 UID；decode OUTDIR 只解码已有文件。
use eha_sdk::{
    can::{CanChannelFactory, CanConnector, CanOptions, python::PythonCanOptions},
    host::{Client, Reply, Wait},
    protocol::{
        Response,
        responses::{ConfigView, TargetMode},
    },
    usb,
};
use std::{
    error::Error,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
enum Connector {
    Usb(usb::UsbConnector),
    Can(CanConnector),
}
impl Connector {
    fn open(&mut self) -> Result<eha_sdk::host::backend::Backend, Box<dyn Error>> {
        match self {
            Self::Usb(v) => Ok(v.open()?),
            Self::Can(v) => Ok(v.open()?),
        }
    }
}
fn fields(response: Response<'_>) -> String {
    match response {
        Response::Identity(v) => format!("{:?}", v.fields()),
        Response::Status(v) | Response::Telemetry(v) => format!("{:?}", v.fields()),
        Response::Measurements(v) => format!("{:?}", v.fields()),
        Response::Diagnostics(v) => format!(
            "overflow={} entries={:?}",
            v.overflow(),
            v.entries().map(|v| v.fields()).collect::<Vec<_>>()
        ),
        Response::ConfigData(v) => format!("{:?}", v.fields()),
        Response::OperationResult(v) => format!("{:?}", v.fields()),
        Response::DataUnavailable(v) => format!("{:?}", v.fields()),
    }
}
fn record(out: &std::path::Path, label: &str, reply: &Reply) -> Result<(), Box<dyn Error>> {
    std::fs::write(out.join(format!("{label}.bin")), reply.bytes())?;
    println!(
        "{}",
        serde_json::json!({"event":label,"bytes":reply.bytes().len(),"reply":fields(reply.response()?)})
    );
    Ok(())
}
fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|s| s == "decode") {
        for entry in std::fs::read_dir(args.get(1).ok_or("decode DIR")?)? {
            let path = entry?.path();
            if path.extension().is_some_and(|s| s == "bin")
                && !path.to_string_lossy().ends_with("-record.bin")
            {
                let bytes = std::fs::read(&path)?;
                if let Ok(eha_sdk::protocol::Message::Response(r)) =
                    eha_sdk::protocol::decode(&bytes, eha_sdk::protocol::Direction::FirmwareToHost)
                {
                    println!(
                        "{}",
                        serde_json::json!({"event":path.file_stem().unwrap_or_default().to_string_lossy(),"bytes":bytes.len(),"reply":fields(r)})
                    );
                }
            }
        }
        return Ok(());
    }
    let (mut connector,out,mode)=match args.first().map(String::as_str){
        Some("usb") if args.len()>=3=>(Connector::Usb(usb::UsbConnector::new(&args[1])?),PathBuf::from(&args[2]),args.get(3).map(String::as_str)),
        Some("can") if args.len()>=5=>{
            let mode=match args[3].as_str(){"classic"=>eha_sdk::transport::can::Mode::Classic,"fd"=>eha_sdk::transport::can::Mode::Fd,_=>return Err("CAN MODE 必须为 classic 或 fd".into())};
            let (python, operation)=match args.get(5).map(String::as_str){
                Some(value @ ("inspect"|"reconnect-readonly"|"save-current")) => (None, Some(value)),
                Some(value) if value.starts_with("result:") => (None, Some(value)),
                Some(python) => (Some(python.to_owned()), args.get(6).map(String::as_str)),
                None => (None, None),
            };
            let factory: Arc<dyn CanChannelFactory>=Arc::new(match python {Some(python)=>PythonCanOptions::with_python(args[1].clone(),python),None=>PythonCanOptions::new(args[1].clone())});
            (Connector::Can(CanConnector::new(CanOptions{node:args[2].parse()?,mode},factory)?),PathBuf::from(&args[4]),operation)},
        _=>return Err("board_check usb SERIAL OUTDIR [save-current|reconnect] | can CONTEXT NODE MODE OUTDIR [PYTHON] [save-current|reconnect]".into()),
    };
    std::fs::create_dir_all(&out)?;
    let mut client = Client::new(connector.open()?);
    let wait = Wait::new(Duration::from_secs(35));
    let expected_uid = std::env::var("EHA_EXPECT_UID")
        .ok()
        .map(|text| -> Result<[u8; 12], Box<dyn Error>> {
            if text.len() != 24 || !text.is_ascii() {
                return Err("EHA_EXPECT_UID 必须为24个十六进制字符".into());
            }
            let mut uid = [0; 12];
            for (i, byte) in uid.iter_mut().enumerate() {
                *byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16)?;
            }
            Ok(uid)
        })
        .transpose()?;
    let identity = client.identify(expected_uid, &wait)?;
    record(&out, "identity", &identity)?;
    record(&out, "status-before", &client.status(&wait)?)?;
    record(&out, "measurements", &client.measurements(&wait)?)?;
    record(&out, "diagnostics", &client.diagnostics(&wait)?)?;
    let mut user = None;
    for (label, view) in [
        ("factory", ConfigView::Factory),
        ("user", ConfigView::UserRecord),
        ("startup", ConfigView::Startup),
        ("communication", ConfigView::Communication),
    ] {
        let reply = client.read_config(view, &wait)?;
        record(&out, label, &reply)?;
        if let Response::ConfigData(data) = reply.response()? {
            std::fs::write(out.join(format!("{label}-record.bin")), data.data())?;
            if view == ConfigView::UserRecord {
                user = Some(data.data().to_vec());
            }
        }
    }
    if mode.is_some_and(|mode| {
        mode == "inspect" || mode == "reconnect-readonly" || mode.starts_with("result:")
    }) {
        if let Some(text) = mode.and_then(|mode| mode.strip_prefix("result:")) {
            if text.len() != 72 || !text.is_ascii() {
                return Err("result: 后须为72个十六进制字符".into());
            }
            let mut bytes = [0; 36];
            for (i, byte) in bytes.iter_mut().enumerate() {
                *byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16)?;
            }
            let key = eha_sdk::session::MaintenanceKey::from_bytes(&bytes)
                .map_err(|e| format!("{e:?}"))?;
            client.track_operation(key)?;
            match client.read_result(key, &wait) {
                Ok(reply) => record(&out, "recovered-result", &reply)?,
                Err(error) => println!(
                    "{}",
                    serde_json::json!({"event":"recovered-result-error","error":format!("{error:?}")})
                ),
            }
        }
        if mode == Some("reconnect-readonly") {
            let state = client.disconnect();
            client = Client::resume(connector.open()?, state);
            record(&out, "identity-reconnected", &client.identify(None, &wait)?)?;
            record(&out, "status-reconnected", &client.status(&wait)?)?;
        }
        record(&out, "status-final", &client.status(&wait)?)?;
        println!(
            "{}",
            serde_json::json!({"event":"io-diagnostics","transport":format!("{:?}",client.transport_status()),"adapter":format!("{:?}",client.adapter_status())})
        );
        client.close();
        return Ok(());
    }
    client.start_heartbeat()?;
    let until = Instant::now() + Duration::from_secs(2);
    let mut n = 0;
    while Instant::now() < until {
        std::thread::sleep(Duration::from_millis(100));
        if let Some(t) = client.telemetry() {
            record(&out, &format!("telemetry-{n:02}"), &t)?;
            n += 1;
        }
    }
    record(&out, "status-heartbeat", &client.status(&wait)?)?;
    println!(
        "{}",
        serde_json::json!({"event":"heartbeat-local","facts":format!("{:?}",client.heartbeat_status()),"telemetry_samples":n})
    );
    client.stop_heartbeat()?;
    let age = client
        .session()
        .identity()
        .ok_or("缺少身份")?
        .host_contact_max_age_ms;
    std::thread::sleep(Duration::from_millis(u64::from(age) + 100));
    record(&out, "status-contact-expired", &client.status(&wait)?)?;
    // 显式验证停止。本例不创建任何运动目标；无传感器时不能绕过采用门禁。
    println!(
        "{}",
        serde_json::json!({"event":"explicit-stop-local","facts":format!("{:?}",client.stop_control(&wait)?)})
    );
    // 等待固件独立遥测，避免下一条查询在共同缓存覆盖尚未取走的 Stop。
    std::thread::sleep(Duration::from_millis(100));
    let stopped = client.status(&wait)?;
    record(&out, "status-after-stop", &stopped)?;
    if let Response::Status(status) = stopped.response()? {
        let f = status.fields();
        if f.target_mode != TargetMode::None {
            return Err("停止后仍有目标，结束本轮设备操作".into());
        }
    }
    if mode == Some("save-current") {
        if let Response::Status(status) = stopped.response()? {
            let f = status.fields();
            if f.axis_state_raw != 1
                || f.axis_error_raw != 0
                || !f.driver_state.qualified
                || f.driver_state.stale
                || f.facts.retained_result
                || f.facts.unknown_effect
                || f.facts.device_operation_pending
            {
                return Err("维护预检失败：要求实际 Idle、当前无轴错误、无未决操作或结果".into());
            }
        }
        let original = user.ok_or("未取得实际用户记录")?;
        eha_sdk::configuration::validate_json(&original)?;
        // 只改变末尾 JSON 空白，保持实际记录容量及全部业务值，便于逐字节读回。
        let mut candidate = original.clone();
        if candidate.last().is_some_and(u8::is_ascii_whitespace) {
            let last = candidate.last_mut().ok_or("空记录")?;
            *last = if *last == b' ' { b'\n' } else { b' ' };
        } else {
            return Err("原记录末尾没有 JSON 空白，无法保持容量；请显式准备候选配置".into());
        }
        eha_sdk::configuration::validate_json(&candidate)?;
        if serde_json::from_slice::<serde_json::Value>(&original)?
            != serde_json::from_slice::<serde_json::Value>(&candidate)?
        {
            return Err("配置业务值发生变化".into());
        }
        std::fs::write(out.join("candidate.json"), &candidate)?;
        println!(
            "{}",
            serde_json::json!({"event":"save-preflight","original_bytes":original.len(),"candidate_bytes":candidate.len(),"same_json_value":true,"reset":false})
        );
        let saved = match client.save_and_readback(&candidate, &wait) {
            Ok(saved) => saved,
            Err(error) => {
                eprintln!(
                    "{}",
                    serde_json::json!({"event":"save-failed","error":format!("{error:?}"),"adapter":format!("{:?}",client.adapter_status()),"transport":format!("{:?}",client.transport_status())})
                );
                if let Ok(reply) = client.identify(None, &wait) {
                    record(&out, "identity-after-save-failure", &reply)?;
                }
                if let Ok(reply) = client.status(&wait) {
                    record(&out, "status-after-save-failure", &reply)?;
                }
                return Err(error.into());
            }
        };
        record(&out, "saved-readback", &saved.readback)?;
        println!(
            "{}",
            serde_json::json!({"event":"saved","key":format!("{:?}",saved.submission.key),"local":format!("{:?}",saved.submission.local),"firmware":format!("{:?}",saved.result),"exact_readback":true})
        );
        let key = saved.submission.key;
        println!(
            "{}",
            serde_json::json!({"event":"release-local","facts":format!("{:?}",client.release_result(&wait)?)})
        );
        std::thread::sleep(Duration::from_millis(100));
        client.confirm_released(key, &wait)?;
        println!(
            "{}",
            serde_json::json!({"event":"release-confirmed","key":format!("{:?}",key)})
        );
    }
    if mode == Some("reconnect") {
        let state = client.disconnect();
        client = Client::resume(connector.open()?, state);
        record(&out, "identity-reconnected", &client.identify(None, &wait)?)?;
        record(&out, "status-reconnected", &client.status(&wait)?)?;
        println!(
            "{}",
            serde_json::json!({"event":"reconnected-heartbeat","facts":format!("{:?}",client.heartbeat_status())})
        );
        let cancelled = Wait::new(Duration::from_secs(1));
        cancelled.cancellation.cancel();
        let error = client
            .status(&cancelled)
            .err()
            .ok_or("已取消的查询却成功")?;
        println!(
            "{}",
            serde_json::json!({"event":"cancelled-read","error":format!("{error:?}")})
        );
    }
    record(&out, "status-final", &client.status(&wait)?)?;
    println!(
        "{}",
        serde_json::json!({"event":"io-diagnostics","transport":format!("{:?}",client.transport_status()),"adapter":format!("{:?}",client.adapter_status())})
    );
    client.close();
    Ok(())
}

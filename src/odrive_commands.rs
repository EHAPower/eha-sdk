// Copyright The eha_controller Contributors

//! ODrive USB 只读入口；设备协议和子进程期限由 SDK 负责。

use clap::{Args, Subcommand};
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};

#[derive(Args)]
pub struct OdriveCommand {
    /// 安装了 odrive==0.5.1.post0 的 Python；默认 EHA_ODRIVE_PYTHON 或 python3
    #[arg(long)]
    python: Option<PathBuf>,
    /// 单次发现或读取的总期限
    #[arg(long, default_value = "5", value_parser = clap::value_parser!(u64).range(1..=60))]
    timeout_secs: u64,
    /// 输出紧凑 JSON；错误也输出 JSON 并以非零状态退出
    #[arg(long)]
    json: bool,
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// 枚举 ODrive USB 候选，不自动选择
    Devices,
    /// 按明确的十六进制序列号读取状态和错误，不写入任何设备属性
    Status {
        #[arg(long)]
        serial: String,
    },
}

pub(crate) fn python_path(explicit: Option<PathBuf>) -> PathBuf {
    explicit
        .or_else(|| std::env::var_os("EHA_ODRIVE_PYTHON").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("python3"))
}

pub(crate) fn error_json(error: &eha_sdk::odrive::Error) -> Value {
    json!({"ok":false,"source":"odrive_usb","message":error.to_string(),"error":{"kind":error.kind.as_str(),"message":error.to_string(),"detail":error.detail}})
}

pub fn run(command: OdriveCommand) -> Result<(), String> {
    let python = python_path(command.python);
    let timeout = Duration::from_secs(command.timeout_secs);
    let result = match command.action {
        Action::Devices => eha_sdk::odrive::discover(&python, timeout),
        Action::Status { serial } => eha_sdk::odrive::read(&python, &serial, timeout),
    };
    match result {
        Ok(value) => print(&value, command.json),
        Err(error) => {
            print(&error_json(&error), command.json)?;
            Err(error.to_string())
        }
    }
}

fn print(value: &Value, compact: bool) -> Result<(), String> {
    let output = if compact {
        serde_json::to_string(value)
    } else {
        serde_json::to_string_pretty(value)
    }
    .map_err(|error| error.to_string())?;
    println!("{output}");
    Ok(())
}

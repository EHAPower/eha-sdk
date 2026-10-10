// Copyright The eha-sdk Contributors

use clap::{CommandFactory, Parser, Subcommand};
use std::{
    io::{self, IsTerminal},
    num::NonZeroU16,
    path::PathBuf,
    process::ExitCode,
};

mod configuration;
mod build_metadata {
    pub const TOOL_VERSION: &str = env!("EHA_TOOL_VERSION");
}
mod device_commands;
mod devices;
mod odrive_commands;
mod session;
mod shell;
mod webui;

#[derive(Parser)]
#[command(
    version = build_metadata::TOOL_VERSION,
    about = "EHA 控制器维护工具",
    after_help = "在终端中不带参数运行可进入交互模式。首次使用：USB 使用 devices；CAN 先在外部 python-can 配置中准备通道，再以 device 选择的通路执行 inspect 或 status 核对设备。低速试动使用 device ... velocity-test；它会显式 Stop 并等待停止条件。配置检查不访问设备。"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// 进入持久交互命令行，或串行执行脚本／标准输入命令
    Shell {
        /// 从文件逐行执行命令；任一失败即停止，绝不重发。
        #[arg(long, conflicts_with = "batch")]
        script: Option<PathBuf>,
        /// 从标准输入逐行执行命令；任一失败即停止，绝不重发。
        #[arg(long)]
        batch: bool,
    },
    /// 按 JSON Schema 和主机规则检查本地完整配置
    ConfigCheck { file: PathBuf },
    /// 列出 USB 候选设备；不打开设备或发送应用消息
    Devices {
        /// 输出候选的 serial/description JSON 数组，供自动化选择。
        #[arg(long)]
        json: bool,
    },
    /// 通过 Rust SDK 在明确选择的 CAN 或 USB 通路上访问一台设备
    Device(device_commands::DeviceCommand),
    /// 独立读取 ODrive USB 状态，不控制设备或改变 H723 会话
    Odrive(odrive_commands::OdriveCommand),
    /// 启动仅限本机访问、持有持久 SDK 会话的 WebUI
    Webui {
        /// 仅在 127.0.0.1 上监听的端口
        #[arg(long, default_value = "8080")]
        port: NonZeroU16,
        /// 带 odrive==0.5.1.post0 的 Python；省略时使用 EHA_ODRIVE_PYTHON 或 python3
        #[arg(long)]
        odrive_python: Option<PathBuf>,
        /// 记录试验原始数据的目录；省略时使用系统本地数据目录。
        #[arg(long)]
        runs_dir: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    let result = match Cli::parse().command {
        None if !io::stdin().is_terminal() || !io::stdout().is_terminal() => Cli::command()
            .print_help()
            .map_err(|error| error.to_string()),
        None => shell::run(None, false),
        Some(Command::Shell { script, batch }) => shell::run(script.as_deref(), batch),
        Some(Command::ConfigCheck { file }) => configuration::check_file(&file),
        Some(Command::Devices { json }) => devices::discover().map(|devices| {
            if json {
                let rows: Vec<_> = devices
                    .iter()
                    .map(|device| {
                        serde_json::json!({
                            "serial": device.serial,
                            "description": device.description,
                        })
                    })
                    .collect();
                println!("{}", serde_json::Value::Array(rows));
            } else {
                shell::print_devices(&devices);
            }
        }),
        Some(Command::Device(command)) => device_commands::run(command),
        Some(Command::Odrive(command)) => odrive_commands::run(command),
        Some(Command::Webui {
            port,
            odrive_python,
            runs_dir,
        }) => webui::serve(port.get(), odrive_python, runs_dir),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn parse_f32(token: &str) -> Result<f32, String> {
    eha_sdk::config::parse_f32(token).map_err(|error| error.to_string())
}

fn deserialize_f32<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<f32, D::Error> {
    let raw = <Box<serde_json::value::RawValue> as serde::Deserialize>::deserialize(deserializer)?;
    parse_f32(raw.get()).map_err(serde::de::Error::custom)
}

fn deserialize_optional_f32<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<f32>, D::Error> {
    let raw = <Option<Box<serde_json::value::RawValue>> as serde::Deserialize>::deserialize(
        deserializer,
    )?;
    raw.map(|raw| parse_f32(raw.get()))
        .transpose()
        .map_err(serde::de::Error::custom)
}

fn raw_json_fields(
    raw: &str,
) -> Result<std::collections::BTreeMap<String, Box<serde_json::value::RawValue>>, serde_json::Error>
{
    struct FieldVisitor;
    impl<'de> serde::de::Visitor<'de> for FieldVisitor {
        type Value = std::collections::BTreeMap<String, Box<serde_json::value::RawValue>>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("具有唯一字段的 JSON 对象")
        }
        fn visit_map<M: serde::de::MapAccess<'de>>(
            self,
            mut map: M,
        ) -> Result<Self::Value, M::Error> {
            let mut fields = std::collections::BTreeMap::new();
            while let Some((name, value)) =
                map.next_entry::<String, Box<serde_json::value::RawValue>>()?
            {
                if fields.insert(name.clone(), value).is_some() {
                    return Err(serde::de::Error::custom(format!("重复字段：{name}")));
                }
            }
            Ok(fields)
        }
    }
    let mut deserializer = serde_json::Deserializer::from_str(raw);
    let fields = serde::Deserializer::deserialize_map(&mut deserializer, FieldVisitor)?;
    deserializer.end()?;
    Ok(fields)
}

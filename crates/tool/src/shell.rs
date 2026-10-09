// Copyright The eha-sdk Contributors

//! 持久 Shell，所有设备动作经过与 CLI/WebUI 相同的 `ToolSession`。

use crate::{
    configuration,
    devices::{self, Device},
    session::{Command as SessionCommand, ConnectionRequest, ToolSession},
};
use clap::{CommandFactory, Parser, Subcommand};
use rustyline::{
    Config, Context, Editor, Helper,
    completion::{Completer, FilenameCompleter, Pair},
    error::ReadlineError,
    highlight::Highlighter,
    hint::Hinter,
    history::DefaultHistory,
    validate::Validator,
};
use std::{
    fs,
    io::{self, BufRead, IsTerminal},
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

#[derive(Parser)]
#[command(
    name = "eha",
    no_binary_name = true,
    version,
    about = "EHA 持久交互命令",
    after_help = "device connect 后的动作共用同一 Client/Connector；disconnect 不发送 Stop，reconnect 不重发动作。"
)]
struct Input {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 独立的 ODrive USB 只读入口，不改变已有 EHA 会话
    Odrive(crate::odrive_commands::OdriveCommand),
    Device {
        #[command(subcommand)]
        command: DeviceCommand,
    },
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    ConfigCheck {
        file: PathBuf,
    },
    Status,
    Measurements,
    Diagnostics,
    Telemetry,
    HeartbeatStart,
    HeartbeatStop,
    HeartbeatOnce,
    Position {
        mm: f32,
    },
    Velocity {
        mm_s: f32,
    },
    Force {
        n: f32,
    },
    Impedance {
        equilibrium_mm: f32,
        stiffness_n_per_mm: f32,
        damping_ns_per_mm: f32,
    },
    Stop,
    ConfigRead {
        #[arg(long, default_value = "user")]
        view: String,
    },
    ConfigSave {
        file: PathBuf,
    },
    RestoreFactory,
    Result {
        #[arg(long)]
        operation_key: String,
    },
    Release {
        #[arg(long)]
        operation_key: String,
    },
    ResetApplication,
    EnterUpdate,
    /// 纯本地等待；不发送查询、心跳或控制。
    Wait {
        seconds: f32,
    },
    Version,
    #[command(alias = "exit")]
    Quit,
}

#[derive(Subcommand)]
enum DeviceCommand {
    List,
    Select {
        selector: String,
    },
    Show,
    Clear,
    Connect,
    ConnectCan {
        /// 外部 python-can 配置中的通道上下文名。
        channel: String,
        node: u8,
        /// `classic` 或 `fd`；省略时为 `fd`。
        mode: Option<String>,
        /// 运行 python-can 的 Python；省略时使用平台默认解释器。
        python: Option<String>,
    },
    Disconnect,
    Reconnect,
    Snapshot,
}
#[derive(Subcommand)]
enum ConfigCommand {
    Check { file: PathBuf },
    Validate { file: PathBuf },
}

#[derive(Default)]
struct Session {
    listed: Vec<Device>,
    selected: Option<Device>,
    tool: ToolSession,
}

impl Session {
    fn execute(&mut self, command: Command) -> Result<bool, String> {
        let action = match command {
            Command::Odrive(command) => {
                crate::odrive_commands::run(command)?;
                return Ok(true);
            }
            Command::Quit => return Ok(false),
            Command::Version => {
                println!("eha-tool {}", crate::build_metadata::TOOL_VERSION);
                return Ok(true);
            }
            Command::Config {
                command: ConfigCommand::Check { file },
            }
            | Command::ConfigCheck { file } => {
                configuration::check_file(&file)?;
                return Ok(true);
            }
            Command::Config {
                command: ConfigCommand::Validate { file },
            } => {
                let record = read_record(&file)?;
                print_result(
                    &self
                        .tool
                        .execute(SessionCommand::ConfigValidate { record })
                        .map_err(|error| error.to_string())?,
                )?;
                return Ok(true);
            }
            Command::Device { command } => return self.device(command),
            Command::Status => SessionCommand::Status,
            Command::Measurements => SessionCommand::Measurements,
            Command::Diagnostics => SessionCommand::Diagnostics,
            Command::Telemetry => SessionCommand::Telemetry,
            Command::HeartbeatStart => SessionCommand::HeartbeatStart,
            Command::HeartbeatStop => SessionCommand::HeartbeatStop,
            Command::HeartbeatOnce => SessionCommand::HeartbeatOnce,
            Command::Position { mm } => SessionCommand::Position { mm },
            Command::Velocity { mm_s } => SessionCommand::Velocity { mm_s },
            Command::Force { n } => SessionCommand::Force { n },
            Command::Impedance {
                equilibrium_mm,
                stiffness_n_per_mm,
                damping_ns_per_mm,
            } => SessionCommand::Impedance {
                equilibrium_mm,
                stiffness_n_per_mm,
                damping_ns_per_mm,
            },
            Command::Stop => SessionCommand::Stop,
            Command::ConfigRead { view } => SessionCommand::ConfigRead { view },
            Command::ConfigSave { file } => SessionCommand::ConfigSave {
                record: read_record(&file)?,
            },
            Command::RestoreFactory => SessionCommand::RestoreFactory,
            Command::Result { operation_key } => {
                SessionCommand::MaintenanceResult { operation_key }
            }
            Command::Release { operation_key } => {
                SessionCommand::MaintenanceRelease { operation_key }
            }
            Command::ResetApplication => SessionCommand::ResetApplication,
            Command::EnterUpdate => SessionCommand::EnterUpdate,
            Command::Wait { seconds } => {
                if !seconds.is_finite() || seconds < 0.0 {
                    return Err("wait seconds 必须是非负有限值".into());
                }
                thread::sleep(Duration::from_secs_f32(seconds));
                println!(
                    "仅本地等待完成；未发送任何设备消息。\n{}",
                    serde_json::to_string_pretty(&self.tool.snapshot())
                        .map_err(|error| error.to_string())?
                );
                return Ok(true);
            }
        };
        print_result(
            &self
                .tool
                .execute(action)
                .map_err(|error| error.to_string())?,
        )?;
        Ok(true)
    }

    fn device(&mut self, command: DeviceCommand) -> Result<bool, String> {
        match command {
            DeviceCommand::List => {
                self.listed.clear();
                self.listed = devices::discover()?;
                print_devices(&self.listed);
            }
            DeviceCommand::Select { selector } => {
                let serial = if selector.len() < 24 && selector.bytes().all(|c| c.is_ascii_digit())
                {
                    devices::choose(&self.listed, &selector)?
                        .serial
                        .ok_or("设备缺少 USB 序列号，不能选择")?
                } else {
                    selector
                };
                self.selected = Some(devices::find_serial(&devices::discover()?, &serial)?);
                println!(
                    "已选择 USB 对象：{}。使用 device connect 才会核对应用身份。",
                    serial.escape_debug()
                );
            }
            DeviceCommand::Show => match &self.selected {
                Some(device) => println!("本地 USB 选择：{}", label(device)),
                None => println!("尚未选择 USB 对象。"),
            },
            DeviceCommand::Clear => {
                self.selected = None;
                println!("已清除本地 USB 选择；现有会话不受影响。")
            }
            DeviceCommand::Connect => {
                let serial = self
                    .selected
                    .as_ref()
                    .and_then(|device| device.serial.clone())
                    .ok_or("请先 device select 一个带完整序列号的 USB 对象")?;
                print_snapshot(
                    self.tool
                        .connect(ConnectionRequest::Usb { serial })
                        .map_err(|error| error.to_string())?,
                )?;
            }
            DeviceCommand::ConnectCan {
                channel,
                node,
                mode,
                python,
            } => print_snapshot(
                self.tool
                    .connect(ConnectionRequest::Can {
                        channel,
                        node,
                        mode: mode.unwrap_or_else(|| "fd".into()),
                        python,
                    })
                    .map_err(|error| error.to_string())?,
            )?,
            DeviceCommand::Disconnect => {
                print_snapshot(self.tool.disconnect().map_err(|error| error.to_string())?)?
            }
            DeviceCommand::Reconnect => {
                print_snapshot(self.tool.reconnect().map_err(|error| error.to_string())?)?
            }
            DeviceCommand::Snapshot => print_snapshot(self.tool.snapshot())?,
        }
        Ok(true)
    }
    fn prompt(&self) -> String {
        match &self.selected {
            Some(device) => format!("eha[{}]> ", label(device)),
            None => "eha[未选择]> ".into(),
        }
    }
}

fn read_record(file: &Path) -> Result<String, String> {
    fs::read_to_string(file)
        .map_err(|error| format!("配置文件读取失败 `{}`: {error}", file.display()))
}
fn label(device: &Device) -> String {
    device
        .serial
        .as_deref()
        .unwrap_or("无序列号")
        .escape_debug()
        .to_string()
}
pub fn print_devices(devices: &[Device]) {
    if devices.is_empty() {
        println!("未发现 EHA USB 候选设备。仍可执行本地配置检查。");
        return;
    }
    for (index, device) in devices.iter().enumerate() {
        println!(
            "{}  {}  {}",
            index + 1,
            label(device),
            device.description.escape_debug()
        );
    }
    println!("以上为 USB 枚举信息，未核对应用固件身份或运行状态。")
}
fn print_result(result: &crate::session::CommandResult) -> Result<(), String> {
    println!(
        "{}",
        serde_json::to_string_pretty(result).map_err(|error| error.to_string())?
    );
    Ok(())
}
fn print_snapshot(snapshot: crate::session::Snapshot) -> Result<(), String> {
    println!(
        "{}",
        serde_json::to_string_pretty(&snapshot).map_err(|error| error.to_string())?
    );
    Ok(())
}

pub fn run(script: Option<&Path>, batch: bool) -> Result<(), String> {
    if let Some(path) = script {
        return run_batch(io::BufReader::new(fs::File::open(path).map_err(
            |error| format!("无法读取脚本 `{}`: {error}", path.display()),
        )?));
    }
    if batch {
        return run_batch(io::stdin().lock());
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err("交互模式需要终端；使用 `shell --batch` 从标准输入串行执行。".into());
    }
    let config = Config::builder()
        .max_history_size(100)
        .map_err(|error| error.to_string())?
        .build();
    let mut editor = Editor::<ShellHelper, DefaultHistory>::with_config(config)
        .map_err(|error| format!("无法初始化终端：{error}"))?;
    editor.set_helper(Some(ShellHelper {
        files: FilenameCompleter::new(),
    }));
    let mut session = Session::default();
    println!(
        "EHA 持久交互工具 {}\n输入 help 查看命令；device connect 后动作会复用同一会话。",
        crate::build_metadata::TOOL_VERSION
    );
    loop {
        let line = match editor.readline(&session.prompt()) {
            Ok(line) => line,
            Err(ReadlineError::Interrupted) => continue,
            Err(ReadlineError::Eof) => break,
            Err(error) => return Err(format!("终端读取失败：{error}")),
        };
        if line.trim().is_empty() {
            continue;
        }
        editor
            .add_history_entry(line.as_str())
            .map_err(|error| error.to_string())?;
        match execute_line(&mut session, &line) {
            Ok(false) => break,
            Ok(true) => {}
            Err(error) => eprintln!("error: {error}"),
        }
    }
    println!("已退出工具；未隐式发送停止、取消或复位。");
    Ok(())
}
fn run_batch(reader: impl BufRead) -> Result<(), String> {
    let mut session = Session::default();
    for (number, line) in reader.lines().enumerate() {
        let line = line.map_err(|error| format!("读取批处理输入失败：{error}"))?;
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        if !execute_line(&mut session, &line)
            .map_err(|error| format!("第 {} 行：{error}", number + 1))?
        {
            break;
        }
    }
    Ok(())
}
fn execute_line(session: &mut Session, line: &str) -> Result<bool, String> {
    let arguments = shlex::split(line).ok_or("引号未闭合或转义不完整；本行未执行")?;
    if arguments.is_empty() {
        return Ok(true);
    }
    let input = Input::try_parse_from(arguments).map_err(|error| error.to_string())?;
    session.execute(input.command)
}
struct ShellHelper {
    files: FilenameCompleter,
}
impl Helper for ShellHelper {}
impl Hinter for ShellHelper {
    type Hint = String;
}
impl Highlighter for ShellHelper {}
impl Validator for ShellHelper {}
impl Completer for ShellHelper {
    type Candidate = Pair;
    fn complete(
        &self,
        line: &str,
        pos: usize,
        context: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let before = &line[..pos];
        let start = before
            .char_indices()
            .rev()
            .find(|(_, character)| character.is_whitespace())
            .map_or(0, |(index, character)| index + character.len_utf8());
        let mut command = Input::command();
        command.build();
        if let Some(words) = shlex::split(&before[..start]) {
            let mut current = &command;
            for word in &words {
                let Some(child) = current.find_subcommand(word) else {
                    return self.files.complete(line, pos, context);
                };
                current = child;
            }
            let pairs: Vec<_> = current
                .get_subcommands()
                .filter(|child| {
                    !child.is_hide_set() && child.get_name().starts_with(&before[start..])
                })
                .map(|child| Pair {
                    display: child.get_name().into(),
                    replacement: format!("{} ", child.get_name()),
                })
                .collect();
            if !pairs.is_empty() {
                return Ok((start, pairs));
            }
        }
        self.files.complete(line, pos, context)
    }
}

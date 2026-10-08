// Copyright The eha-tool Contributors

//! CLI 的本地配置检查输出；校验规则由 `eha_sdk::configuration` 唯一维护。

/// 单次命令和交互命令共用的本地检查入口，不保留候选或访问设备。
pub fn check_file(path: &std::path::Path) -> Result<(), String> {
    let display = path.display();
    let bytes =
        std::fs::read(path).map_err(|error| format!("配置文件读取失败 `{display}`: {error}"))?;
    eha_sdk::configuration::validate_json(&bytes)
        .map_err(|error| format!("配置参数校验失败 `{display}`: {error}"))?;
    println!("静态配置校验通过：`{display}`。未核对设备或目标固件匹配，结果不表示可以部署。");
    Ok(())
}

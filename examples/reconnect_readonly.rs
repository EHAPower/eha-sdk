// Copyright The eha_controller Contributors
//! 只读地演示同一 USB Connector 上的显式连接恢复。
//! 用法：reconnect_readonly usb SERIAL
use eha_sdk::{
    host::{Client, Wait},
    usb::UsbConnector,
};
use std::{error::Error, time::Duration};

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let [transport, serial] = args.as_slice() else {
        return Err("用法：reconnect_readonly usb <完整 USB 序列号>".into());
    };
    if transport != "usb" {
        return Err("本示例只支持 USB；用法：reconnect_readonly usb <完整 USB 序列号>".into());
    }

    let mut connector = UsbConnector::new(serial)?;
    let wait = Wait::new(Duration::from_secs(5));
    let mut client = Client::new(connector.open()?);
    let first = client.identify(None, &wait)?;
    println!("初次 Identity：{:?}", first.response()?);
    println!("初次 Status：{:?}", client.status(&wait)?.response()?);

    // `disconnect` 只保留业务关联并回收本地 I/O；它不发送 Stop、Reset 或心跳。
    let state = client.disconnect();
    let mut client = Client::resume(connector.open()?, state);
    let resumed = client.identify(None, &wait)?;
    println!("恢复后 Identity：{:?}", resumed.response()?);
    println!("恢复后 Status：{:?}", client.status(&wait)?.response()?);
    println!("本地连接状态：{:?}", client.connection_status());
    client.close();
    Ok(())
}

// Copyright The eha-sdk Contributors
//! 旧 WebUI 命令和静态页面的兼容适配器。

use crate::service::Method;
use std::path::PathBuf;

pub(crate) fn serve(
    port: u16,
    odrive_python: Option<PathBuf>,
    runs_dir: Option<PathBuf>,
    integration_token_file: Option<PathBuf>,
) -> Result<(), String> {
    crate::service::serve_webui(port, odrive_python, runs_dir, integration_token_file)
}

pub(crate) struct StaticResponse {
    pub(crate) status: u16,
    pub(crate) content_type: &'static str,
    pub(crate) content_length: usize,
    pub(crate) body: Vec<u8>,
}
struct Asset {
    route: &'static str,
    content_type: &'static str,
    bytes: &'static [u8],
}
const ASSETS: &[Asset] = &[
    Asset {
        route: "/",
        content_type: "text/html; charset=utf-8",
        bytes: include_bytes!("../webui/static/index.html"),
    },
    Asset {
        route: "/index.html",
        content_type: "text/html; charset=utf-8",
        bytes: include_bytes!("../webui/static/index.html"),
    },
    Asset {
        route: "/styles.css",
        content_type: "text/css; charset=utf-8",
        bytes: include_bytes!("../webui/static/styles.css"),
    },
    Asset {
        route: "/app.js",
        content_type: "application/javascript; charset=utf-8",
        bytes: include_bytes!("../webui/static/app.js"),
    },
    Asset {
        route: "/telemetry.js",
        content_type: "application/javascript; charset=utf-8",
        bytes: include_bytes!("../webui/static/telemetry.js"),
    },
    Asset {
        route: "/config-editor.js",
        content_type: "application/javascript; charset=utf-8",
        bytes: include_bytes!("../webui/static/config-editor.js"),
    },
    Asset {
        route: "/trial-ui.js",
        content_type: "application/javascript; charset=utf-8",
        bytes: include_bytes!("../webui/static/trial-ui.js"),
    },
    Asset {
        route: "/trial-state.js",
        content_type: "application/javascript; charset=utf-8",
        bytes: include_bytes!("../webui/static/trial-state.js"),
    },
    Asset {
        route: "/recording-ui.js",
        content_type: "application/javascript; charset=utf-8",
        bytes: include_bytes!("../webui/static/recording-ui.js"),
    },
    Asset {
        route: "/can-connection-ui.js",
        content_type: "application/javascript; charset=utf-8",
        bytes: include_bytes!("../webui/static/can-connection-ui.js"),
    },
    Asset {
        route: "/favicon.svg",
        content_type: "image/svg+xml",
        bytes: include_bytes!("../webui/static/favicon.svg"),
    },
    Asset {
        route: "/vendor/echarts.min.js",
        content_type: "application/javascript; charset=utf-8",
        bytes: include_bytes!("../webui/static/vendor/echarts.min.js"),
    },
    Asset {
        route: "/vendor/echarts.LICENSE.txt",
        content_type: "text/plain; charset=utf-8",
        bytes: include_bytes!("../webui/static/vendor/echarts.LICENSE.txt"),
    },
    Asset {
        route: "/vendor/echarts.LICENSE-d3.txt",
        content_type: "text/plain; charset=utf-8",
        bytes: include_bytes!("../webui/static/vendor/echarts.LICENSE-d3.txt"),
    },
    Asset {
        route: "/vendor/echarts.NOTICE.txt",
        content_type: "text/plain; charset=utf-8",
        bytes: include_bytes!("../webui/static/vendor/echarts.NOTICE.txt"),
    },
    Asset {
        route: "/vendor/tslib.CopyrightNotice.txt",
        content_type: "text/plain; charset=utf-8",
        bytes: include_bytes!("../webui/static/vendor/tslib.CopyrightNotice.txt"),
    },
    Asset {
        route: "/vendor/tslib.LICENSE.txt",
        content_type: "text/plain; charset=utf-8",
        bytes: include_bytes!("../webui/static/vendor/tslib.LICENSE.txt"),
    },
    Asset {
        route: "/vendor/zrender.LICENSE.txt",
        content_type: "text/plain; charset=utf-8",
        bytes: include_bytes!("../webui/static/vendor/zrender.LICENSE.txt"),
    },
];
pub(crate) fn static_response(method: Method, path: &str) -> StaticResponse {
    if !matches!(method, Method::Get | Method::Head) {
        return StaticResponse {
            status: 405,
            content_type: "text/plain; charset=utf-8",
            content_length: 19,
            body: b"method not allowed\n".to_vec(),
        };
    }
    let Some(asset) = ASSETS.iter().find(|asset| asset.route == path) else {
        return StaticResponse {
            status: 404,
            content_type: "text/plain; charset=utf-8",
            content_length: 10,
            body: b"not found\n".to_vec(),
        };
    };
    StaticResponse {
        status: 200,
        content_type: asset.content_type,
        content_length: asset.bytes.len(),
        body: if method == Method::Head {
            Vec::new()
        } else {
            asset.bytes.to_vec()
        },
    }
}

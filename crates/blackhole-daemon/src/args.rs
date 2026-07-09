use blackhole_shared::SchemeId;

/// Daemon 命令行参数
#[derive(Debug, Clone, Default)]
pub struct DaemonArgs {
    /// 外部词典文件路径
    pub dict_path: Option<String>,
    /// 强制指定的输入方案
    pub scheme: Option<SchemeId>,
    /// 以 IBus 引擎模式运行（Linux 专用）
    pub ibus: bool,
}

/// 解析命令行参数
pub fn parse_args() -> DaemonArgs {
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    let mut result = DaemonArgs::default();

    while i < args.len() {
        match args[i].as_str() {
            "-d" | "--dict" => {
                i += 1;
                if i < args.len() {
                    result.dict_path = Some(args[i].clone());
                }
            }
            "-s" | "--scheme" => {
                i += 1;
                if i < args.len() {
                    result.scheme = parse_scheme(&args[i]);
                }
            }
            "--ibus" => {
                result.ibus = true;
            }
            _ => {}
        }
        i += 1;
    }

    result
}

fn parse_scheme(s: &str) -> Option<SchemeId> {
    match s.to_lowercase().as_str() {
        "pinyin" | "py" => Some(SchemeId::Pinyin),
        "shuangpin" | "sp" => Some(SchemeId::Shuangpin),
        _ => {
            tracing::warn!("Unknown scheme '{}', expected 'pinyin' or 'shuangpin'", s);
            None
        }
    }
}

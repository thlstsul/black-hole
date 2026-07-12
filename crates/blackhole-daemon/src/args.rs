use blackhole_shared::SchemeId;
use clap::Parser;

/// Blackhole IME 守护进程命令行参数
#[derive(Debug, Clone, Parser)]
#[command(version, about = "Blackhole IME - 跨平台中文输入法守护进程")]
pub struct DaemonArgs {
    /// 外部词典文件路径
    #[arg(short = 'd', long = "dict", value_name = "PATH")]
    pub dict_path: Option<String>,

    /// 强制指定的输入方案 (pinyin / shuangpin)
    #[arg(short = 's', long = "scheme", value_name = "SCHEME", value_parser = parse_scheme)]
    pub scheme: Option<SchemeId>,

    /// 以 IBus 引擎模式运行（Linux 专用）
    #[arg(long = "ibus")]
    pub ibus: bool,
}

fn parse_scheme(s: &str) -> Result<SchemeId, String> {
    match s.to_lowercase().as_str() {
        "pinyin" | "py" => Ok(SchemeId::Pinyin),
        "shuangpin" | "sp" => Ok(SchemeId::Shuangpin),
        _ => Err(format!(
            "Unknown scheme '{s}'. Expected one of: pinyin, py, shuangpin, sp"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_scheme_pinyin() {
        assert_eq!(parse_scheme("pinyin"), Ok(SchemeId::Pinyin));
        assert_eq!(parse_scheme("py"), Ok(SchemeId::Pinyin));
        assert_eq!(parse_scheme("PINYIN"), Ok(SchemeId::Pinyin));
    }

    #[test]
    fn parse_scheme_shuangpin() {
        assert_eq!(parse_scheme("shuangpin"), Ok(SchemeId::Shuangpin));
        assert_eq!(parse_scheme("sp"), Ok(SchemeId::Shuangpin));
        assert_eq!(parse_scheme("SHUANGPIN"), Ok(SchemeId::Shuangpin));
    }

    #[test]
    fn parse_scheme_invalid() {
        assert!(parse_scheme("unknown").is_err());
        assert!(parse_scheme("").is_err());
    }

    #[test]
    fn daemon_args_default() {
        let args = DaemonArgs::parse_from(["blackhole"]);
        assert!(args.dict_path.is_none());
        assert!(args.scheme.is_none());
        assert!(!args.ibus);
    }

    #[test]
    fn daemon_args_dict() {
        let args = DaemonArgs::parse_from(["blackhole", "-d", "/path/to/dict.yaml"]);
        assert_eq!(args.dict_path.as_deref(), Some("/path/to/dict.yaml"));
    }

    #[test]
    fn daemon_args_scheme() {
        let args = DaemonArgs::parse_from(["blackhole", "-s", "shuangpin"]);
        assert_eq!(args.scheme, Some(SchemeId::Shuangpin));
    }

    #[test]
    fn daemon_args_ibus() {
        let args = DaemonArgs::parse_from(["blackhole", "--ibus"]);
        assert!(args.ibus);
    }

    #[test]
    fn daemon_args_long_dict() {
        let args = DaemonArgs::parse_from(["blackhole", "--dict", "dict.yaml"]);
        assert_eq!(args.dict_path.as_deref(), Some("dict.yaml"));
    }

    #[test]
    fn daemon_args_long_scheme() {
        let args = DaemonArgs::parse_from(["blackhole", "--scheme", "pinyin"]);
        assert_eq!(args.scheme, Some(SchemeId::Pinyin));
    }
}

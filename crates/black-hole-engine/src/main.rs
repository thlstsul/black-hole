use black_hole_engine::EngineBuilder;
use black_hole_shared::{
    EngineCommand, InputContext, KeyEvent, KeyState, Modifiers, SchemeId, SchemeResult,
};
use std::env;
use std::io::{self, Write};
use tracing::info;
use tracing_subscriber::fmt;

fn main() {
    fmt().with_target(false).without_time().init();

    let args: Vec<String> = env::args().collect();
    let mut dict_path: Option<&str> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-d" | "--dict" => {
                i += 1;
                if i < args.len() {
                    dict_path = Some(&args[i]);
                }
            }
            "-h" | "--help" => {
                println!("Black-Hole IME CLI Test Harness");
                println!("Usage: black-hole-cli [OPTIONS]");
                println!();
                println!("Options:");
                println!("  -d, --dict <PATH>  Load RIME dictionary file (.dict.yaml or .txt)");
                println!("  -h, --help         Print help");
                println!();
                println!("Commands:");
                println!("  :pinyin    Switch to Pinyin scheme");
                println!("  :shuangpin Switch to Shuangpin scheme");
                println!("  :quit      Exit");
                return;
            }
            _ => {}
        }
        i += 1;
    }

    info!("Black-Hole IME CLI Test Harness");
    info!("Commands: :pinyin | :shuangpin | :quit");
    info!("Input code and press Enter.");
    if let Some(path) = dict_path {
        info!("Loading RIME dictionary from: {}", path);
    }

    let mut current_scheme = SchemeId::Pinyin;

    let mut builder = EngineBuilder::new().scheme(current_scheme);
    if let Some(path) = dict_path {
        builder = builder.dictionary(path);
    }
    let mut engine = builder.build();
    let ctx = InputContext {
        caret_x: 0,
        caret_y: 0,
        caret_h: 0,
    };

    loop {
        print!("[{}] > ", engine.current_scheme_name());
        io::stdout().flush().unwrap();

        let mut line = String::new();
        if io::stdin().read_line(&mut line).is_err() {
            break;
        }
        let line = line.trim();

        if line == "quit" || line == "exit" || line == ":quit" {
            break;
        }

        if line.is_empty() {
            continue;
        }

        // 方案切换命令
        match line {
            ":pinyin" => {
                current_scheme = SchemeId::Pinyin;
                engine.process(&EngineCommand::SwitchScheme(current_scheme), &ctx);
                info!("Switched to 拼音");
                continue;
            }
            ":shuangpin" => {
                current_scheme = SchemeId::Shuangpin;
                engine.process(&EngineCommand::SwitchScheme(current_scheme), &ctx);
                info!("Switched to 小鹤双拼");
                continue;
            }
            _ => {}
        }

        // 将输入的每个字符作为按键发送给引擎
        for ch in line.chars() {
            let key = KeyEvent {
                key: ch.to_string(),
                modifiers: Modifiers {
                    shift: false,
                    ctrl: false,
                    alt: false,
                    meta: false,
                    capslock: false,
                },
                state: KeyState::Press,
            };
            let result = engine.process(&EngineCommand::Key(key), &ctx);
            match result {
                SchemeResult::Composing {
                    code,
                    candidates,
                    selected_index,
                    ..
                } => {
                    info!("  composing: {}", code);
                    for (i, c) in candidates.iter().enumerate() {
                        let marker = if i == selected_index { ">" } else { " " };
                        info!("    {} {}. {}", marker, i + 1, c.text);
                    }
                    if candidates.is_empty() {
                        info!("    (no candidates)");
                    }
                }
                SchemeResult::Committed { text } => {
                    info!("  committed: {}", text);
                }
                SchemeResult::Ignored => {
                    // 忽略非字母输入
                }
            }
        }

        // 最后发送 Enter 提交
        let enter_key = KeyEvent {
            key: "Enter".to_string(),
            modifiers: Modifiers {
                shift: false,
                ctrl: false,
                alt: false,
                meta: false,
                capslock: false,
            },
            state: KeyState::Press,
        };
        let result = engine.process(&EngineCommand::Key(enter_key), &ctx);
        if let SchemeResult::Committed { text } = result {
            info!("  committed: {}", text);
        }

        // 重置引擎状态，准备下一行输入
        engine.process(&EngineCommand::Reset, &ctx);
        info!("");
    }

    info!("Goodbye!");
}

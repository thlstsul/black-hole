// 性能回归测试：验证双拼（小鹤）方案长输入"输入越多越卡顿"问题已修复。
//
// 历史问题：双拼每次按键会对无空格的连续全拼 `full_code` 做前缀查询，
// 枚举全部切分路径并逐路径查表；且每次冷查询都会重新解析 58MB 词典表
// （Table::load 约 3-7ms/次）。长输入下单键可达数百 ms 至数秒。
// 修复：用 self_cell 缓存解析后的 Table/Prism，冷查询不再重复解析。
use black_hole_engine::{InputScheme, RimeDict, ShuangpinScheme, default_user_dict_dir};
use black_hole_shared::{InputContext, KeyEvent, KeyState, Modifiers};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn key_event(key: &str) -> KeyEvent {
    KeyEvent {
        key: key.to_string(),
        modifiers: Modifiers {
            shift: false,
            ctrl: false,
            alt: false,
            meta: false,
            capslock: false,
        },
        state: KeyState::Press,
    }
}

fn ctx() -> InputContext {
    InputContext::caret(0, 0, 20)
}

/// 加载真实词库（rime_ice，复用 daemon 的缓存目录）
fn real_dict() -> Arc<RimeDict> {
    let dict_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../assets/dicts/rime_ice.dict.yaml"
    );
    let cache_dir = default_user_dict_dir().join("cache");
    Arc::new(
        RimeDict::from_rime_dict_cached(dict_path, &cache_dir)
            .expect("failed to load rime_ice dict"),
    )
}

/// 逐键输入，返回每键耗时（µs）。scheme 在轮次间复位。
fn type_keys(scheme: &mut ShuangpinScheme, input: &str) -> Vec<Duration> {
    let c = ctx();
    scheme.reset();
    let mut per_key = Vec::new();
    for ch in input.chars() {
        let k = ch.to_string();
        let t = Instant::now();
        let _ = scheme.handle_key(&key_event(&k), &c);
        per_key.push(t.elapsed());
    }
    per_key
}

fn report(input: &str, label: &str, per_key: &[Duration]) {
    let total: Duration = per_key.iter().sum();
    let avg = total.as_secs_f64() * 1000.0 / per_key.len() as f64;
    let max = per_key.iter().map(|d| d.as_secs_f64()).fold(0.0, f64::max) * 1000.0;
    println!(
        "[{}] '{}' 总耗时 {:.1}ms  平均 {:.2}ms/键  最大 {:.2}ms",
        label,
        input,
        total.as_secs_f64() * 1000.0,
        avg,
        max
    );
}

#[test]
fn bench_shuangpin_long_input() {
    let mut scheme = ShuangpinScheme::with_dictionary(Box::new(real_dict()));

    // 同一句话不断延长：中华人民共和国 = vshx rf mb gs he go
    let base = "vshxrfmbgshego"; // 14 键 / 7 音节
    for end in [6usize, 10, 14] {
        let per_key = type_keys(&mut scheme, &base[..end]);
        report(&base[..end], "增长", &per_key);
    }

    // 自然长句：今天我们一起吃饭吧 = jb tm wo mf yi qi ii fj ba
    let sentence = "jbtmwomfyiqiifjba"; // 17 键 / 9 音节
    let per_key = type_keys(&mut scheme, sentence);
    report(sentence, "长句", &per_key);

    // 极长输入：42 键 / 21 音节（三遍"中华人民共和国"）
    let longest = "vshxrfmbgshegovshxrfmbgshegovshxrfmbgshego";
    let per_key = type_keys(&mut scheme, longest);
    report(longest, "极长", &per_key);

    // 回归断言：排除冷启动首键后，42 键输入逐键平均应可控。
    // 修复前该输入平均约 990ms/键（最大 3s），阈值放宽到 50ms 仍能抓住回归。
    let steady: Vec<_> = per_key.iter().skip(1).collect();
    let avg_ms = steady.iter().map(|d| d.as_secs_f64()).sum::<f64>() / steady.len() as f64 * 1000.0;
    println!("[极长] 除首键外平均 {:.2}ms/键", avg_ms);
    assert!(
        avg_ms < 50.0,
        "42 键双拼输入逐键平均 {:.2}ms 过高，可能存在随输入长度增长的性能回归",
        avg_ms
    );
}

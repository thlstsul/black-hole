// 性能回归测试：验证长输入（输入越多越卡顿）问题已修复。
//
// 历史问题：`PinyinCodec::rebuild` 每次按键都全量重算切分（dp_segment），
// 且路径用 Vec<String> 深拷贝存储，开销 O(n²×50) 随输入长度二次增长——
// 39 字输入时单键可达 30-80ms。修复后路径改为共享尾部链表（Rc），
// 每次按键降为 O(n×6×50) 的轻量节点分配。
use black_hole_engine::{InputScheme, PinyinScheme, RimeDict, default_user_dict_dir};
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
    InputContext {
        caret_x: 0,
        caret_y: 0,
        caret_h: 20,
    }
}

/// 加载真实词库（rime_ice，复用 daemon 的缓存目录）
fn real_dict() -> Arc<RimeDict> {
    let dict_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../assets/dicts/rime_ice.dict.yaml"
    );
    let cache_dir = default_user_dict_dir().join("cache");
    Arc::new(
        RimeDict::from_rime_dict_cached(dict_path, &cache_dir).expect("failed to load rime_ice dict"),
    )
}

/// 逐键输入，返回每键耗时（µs）。scheme 在轮次间复位，避免输入累积。
fn type_keys(scheme: &mut PinyinScheme, input: &str) -> Vec<Duration> {
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
fn bench_long_input_growth() {
    let mut scheme = PinyinScheme::with_dictionary(real_dict());

    // 同一句话不断延长：若每键耗时随长度二次增长（旧实现特征），
    // 末段按键会显著变慢。
    let base = "zhonghuarenmingongheguo"; // 中 华 人 民 共 和 国
    for end in [6usize, 10, 14, 18] {
        let per_key = type_keys(&mut scheme, &base[..end]);
        report(&base[..end], "增长", &per_key);
    }

    // 歧义多的输入（每段都有多种切分）
    let ambiguous = "zhuangshizhuangshizhuangshi";
    let per_key = type_keys(&mut scheme, ambiguous);
    report(ambiguous, "歧义", &per_key);

    // 典型长句
    let sentence = "jinwanwomenquchifanhaoma";
    let per_key = type_keys(&mut scheme, sentence);
    report(sentence, "长句", &per_key);

    // 极长输入：回归断言目标（旧实现 39 字输入末键可达 30-80ms）
    let longest = "womenshiweidazuozhongguomengdefendoushi";
    let per_key = type_keys(&mut scheme, longest);
    report(longest, "极长", &per_key);

    // 排除冷启动首键后，39 字输入逐键平均应远低于旧实现的 33ms/键。
    // 阈值放宽到 20ms 兼顾不同机器/词典缓存状态，仍能抓住二次增长回归。
    let steady: Vec<_> = per_key.iter().skip(1).collect();
    let avg_ms =
        steady.iter().map(|d| d.as_secs_f64()).sum::<f64>() / steady.len() as f64 * 1000.0;
    println!("[极长] 除首键外平均 {:.2}ms/键", avg_ms);
    assert!(
        avg_ms < 20.0,
        "39 字输入逐键平均 {:.2}ms 过高，可能存在随输入长度增长的性能回归",
        avg_ms
    );
}

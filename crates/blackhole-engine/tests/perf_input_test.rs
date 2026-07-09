/// 性能测试：验证拼音输入卡顿问题是否已修复
use blackhole_engine::{InputScheme, PinyinScheme, SqliteDictionary};
use blackhole_shared::{InputContext, KeyEvent, KeyState, Modifiers};

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

#[test]
fn test_performance_si_input() {
    // 测试输入 "si" 时的性能
    let dict = SqliteDictionary::from_builtin();
    let mut scheme = PinyinScheme::with_dictionary(dict);
    let ctx = InputContext {
        caret_x: 0,
        caret_y: 0,
        caret_h: 20,
    };

    let start = std::time::Instant::now();

    // 输入 "s"
    let _ = scheme.handle_key(&key_event("s"), &ctx);
    // 输入 "i" - 这个字符之前会导致卡顿
    let _ = scheme.handle_key(&key_event("i"), &ctx);

    let elapsed = start.elapsed();

    // 确保处理时间小于 50ms（之前可能会超过 100-200ms）
    assert!(
        elapsed.as_millis() < 50,
        "输入 'si' 耗时过长: {:?}，可能存在性能问题",
        elapsed
    );

    println!("输入 'si' 耗时: {:?}", elapsed);
}

#[test]
fn test_performance_shu_input() {
    // 测试输入 "shu" 时的性能
    let dict = SqliteDictionary::from_builtin();
    let mut scheme = PinyinScheme::with_dictionary(dict);
    let ctx = InputContext {
        caret_x: 0,
        caret_y: 0,
        caret_h: 20,
    };

    let start = std::time::Instant::now();

    // 输入 "s"
    let _ = scheme.handle_key(&key_event("s"), &ctx);
    // 输入 "h"
    let _ = scheme.handle_key(&key_event("h"), &ctx);
    // 输入 "u" - 这个字符之前会导致卡顿
    let _ = scheme.handle_key(&key_event("u"), &ctx);

    let elapsed = start.elapsed();

    // 确保处理时间小于 50ms
    assert!(
        elapsed.as_millis() < 50,
        "输入 'shu' 耗时过长: {:?}，可能存在性能问题",
        elapsed
    );

    println!("输入 'shu' 耗时: {:?}", elapsed);
}

#[test]
fn test_performance_shi_input() {
    // 测试输入 "shi" 时的性能（这是一个特别容易卡顿的例子）
    let dict = SqliteDictionary::from_builtin();
    let mut scheme = PinyinScheme::with_dictionary(dict);
    let ctx = InputContext {
        caret_x: 0,
        caret_y: 0,
        caret_h: 20,
    };

    let start = std::time::Instant::now();

    // 输入 "s"
    let _ = scheme.handle_key(&key_event("s"), &ctx);
    // 输入 "h"
    let _ = scheme.handle_key(&key_event("h"), &ctx);
    // 输入 "i" - 这个字符之前会导致严重卡顿
    let _ = scheme.handle_key(&key_event("i"), &ctx);

    let elapsed = start.elapsed();

    // 确保处理时间小于 50ms
    assert!(
        elapsed.as_millis() < 50,
        "输入 'shi' 耗时过长: {:?}，可能存在性能问题",
        elapsed
    );

    println!("输入 'shi' 耗时: {:?}", elapsed);
}

#[test]
fn test_performance_zhuang_input() {
    // 测试输入 "zhuang" 时的性能（6个字母，容易产生大量切分）
    let dict = SqliteDictionary::from_builtin();
    let mut scheme = PinyinScheme::with_dictionary(dict);
    let ctx = InputContext {
        caret_x: 0,
        caret_y: 0,
        caret_h: 20,
    };

    let start = std::time::Instant::now();

    for ch in ["z", "h", "u", "a", "n", "g"] {
        let _ = scheme.handle_key(&key_event(ch), &ctx);
    }

    let elapsed = start.elapsed();

    // 确保处理时间小于 100ms（6个字母的输入允许稍长时间）
    assert!(
        elapsed.as_millis() < 100,
        "输入 'zhuang' 耗时过长: {:?}，可能存在性能问题",
        elapsed
    );

    println!("输入 'zhuang' 耗时: {:?}", elapsed);
}

#[test]
fn test_performance_delete_si() {
    // 测试删除 "si" 的 "i" 时的性能
    let dict = SqliteDictionary::from_builtin();
    let mut scheme = PinyinScheme::with_dictionary(dict);
    let ctx = InputContext {
        caret_x: 0,
        caret_y: 0,
        caret_h: 20,
    };

    // 先输入 "si"
    let _ = scheme.handle_key(&key_event("s"), &ctx);
    let _ = scheme.handle_key(&key_event("i"), &ctx);

    // 然后删除 "i"
    let start = std::time::Instant::now();
    let _ = scheme.handle_key(&key_event("Backspace"), &ctx);
    let elapsed = start.elapsed();

    // 确保删除操作处理时间小于 10ms
    assert!(
        elapsed.as_millis() < 10,
        "删除 'si' 的 'i' 耗时过长: {:?}，可能存在性能问题",
        elapsed
    );

    println!("删除 'si' 的 'i' 耗时: {:?}", elapsed);
}

#[test]
fn test_performance_delete_shu() {
    // 测试删除 "shu" 的 "u" 时的性能
    let dict = SqliteDictionary::from_builtin();
    let mut scheme = PinyinScheme::with_dictionary(dict);
    let ctx = InputContext {
        caret_x: 0,
        caret_y: 0,
        caret_h: 20,
    };

    // 先输入 "shu"
    let _ = scheme.handle_key(&key_event("s"), &ctx);
    let _ = scheme.handle_key(&key_event("h"), &ctx);
    let _ = scheme.handle_key(&key_event("u"), &ctx);

    // 然后删除 "u"
    let start = std::time::Instant::now();
    let _ = scheme.handle_key(&key_event("Backspace"), &ctx);
    let elapsed = start.elapsed();

    // 确保删除操作处理时间小于 10ms
    assert!(
        elapsed.as_millis() < 10,
        "删除 'shu' 的 'u' 耗时过长: {:?}，可能存在性能问题",
        elapsed
    );

    println!("删除 'shu' 的 'u' 耗时: {:?}", elapsed);
}

#[test]
fn test_performance_delete_shi() {
    // 测试删除 "shi" 的 "i" 时的性能（特别容易卡顿的例子）
    let dict = SqliteDictionary::from_builtin();
    let mut scheme = PinyinScheme::with_dictionary(dict);
    let ctx = InputContext {
        caret_x: 0,
        caret_y: 0,
        caret_h: 20,
    };

    // 先输入 "shi"
    let _ = scheme.handle_key(&key_event("s"), &ctx);
    let _ = scheme.handle_key(&key_event("h"), &ctx);
    let _ = scheme.handle_key(&key_event("i"), &ctx);

    // 然后删除 "i"
    let start = std::time::Instant::now();
    let _ = scheme.handle_key(&key_event("Backspace"), &ctx);
    let elapsed = start.elapsed();

    // 确保删除操作处理时间小于 10ms
    assert!(
        elapsed.as_millis() < 10,
        "删除 'shi' 的 'i' 耗时过长: {:?}，可能存在性能问题",
        elapsed
    );

    println!("删除 'shi' 的 'i' 耗时: {:?}", elapsed);
}

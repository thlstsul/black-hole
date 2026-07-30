// 性能测试：比较 rime-dict 词典的内存占用
use blackhole_engine::Dictionary;
use std::fs::File;
use std::io::Write;
use std::time::Instant;

fn main() {
    println!("=== rime-dict Dictionary Performance Test ===\n");

    // 创建一个大词库测试文件
    let test_dir = std::env::temp_dir().join("blackhole_perf_test");
    std::fs::create_dir_all(&test_dir).unwrap();
    let dict_path = test_dir.join("test_dict.dict.yaml");

    println!("Generating test dictionary with 100,000 entries...");
    let mut file = File::create(&dict_path).unwrap();
    writeln!(file, "---").unwrap();
    writeln!(file, "name: test").unwrap();
    writeln!(file, "version: \"1.0\"").unwrap();
    writeln!(file, "...").unwrap();

    let start = Instant::now();
    for i in 0..100_000 {
        let code = format!("test{:06}", i);
        let text = format!("测试词{}", i);
        writeln!(file, "{}\t{}\t{}", text, code, i).unwrap();
    }
    let gen_time = start.elapsed();
    println!(
        "Dictionary file generated in {:.2}s\n",
        gen_time.as_secs_f64()
    );

    // 测试加载性能
    println!("Loading dictionary...");
    let start = Instant::now();

    match blackhole_engine::RimeDict::from_rime_dict(&dict_path) {
        Ok(dict) => {
            let load_time = start.elapsed();
            println!("Dictionary loaded in {:.2}s\n", load_time.as_secs_f64());

            // 测试查询性能
            println!("Running query tests...");
            let start = Instant::now();
            for i in (0..100_000).step_by(1000) {
                let code = format!("test{:06}", i);
                let _results = dict.lookup(&code);
            }
            let query_time = start.elapsed();
            println!(
                "100 lookups completed in {:.2}ms",
                query_time.as_secs_f64() * 1000.0
            );
            println!(
                "Average query time: {:.2}μs\n",
                query_time.as_secs_f64() * 10_000.0
            );

            // 测试前缀查询
            println!("Running prefix query tests...");
            let start = Instant::now();
            for i in 0..100 {
                let prefix = format!("test{:02}", i);
                let _results = dict.prefix_lookup(&prefix);
            }
            let prefix_query_time = start.elapsed();
            println!(
                "100 prefix lookups completed in {:.2}ms",
                prefix_query_time.as_secs_f64() * 1000.0
            );
            println!(
                "Average prefix query time: {:.2}μs\n",
                prefix_query_time.as_secs_f64() * 10_000.0
            );

            println!("✓ All tests completed successfully!");
        }
        Err(e) => {
            eprintln!("Failed to load dictionary: {}", e);
        }
    }

    // 清理
    let _ = std::fs::remove_file(&dict_path);
    let _ = std::fs::remove_dir(&test_dir);
}

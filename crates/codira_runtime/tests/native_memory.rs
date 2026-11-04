//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 13, 2026
//!
//! Functionality:
//! - Codira programs using the runtime's raw memory services: region arenas
//!   (allocation, quota refusal, bulk reset, stale handles) and OS pages
//!   (writing machine code into a RW page, flipping it to RX, running it).
use codira_test::CompileAndRunTestDriver;

const DECLARATIONS: &str = r#"
extern "C" {
    func codira_region_new(quota: usize) -> usize;
    func codira_region_alloc(region: usize, size: usize, align: usize) -> usize;
    func codira_region_reset(region: usize) -> bool;
    func codira_region_used(region: usize) -> usize;
    func codira_region_free(region: usize) -> bool;
    func codira_page_size() -> usize;
    func codira_page_alloc(size: usize, prot: u32) -> usize;
    func codira_page_protect(addr: usize, size: usize, prot: u32) -> bool;
    func codira_page_free(addr: usize, size: usize) -> bool;
    func codira_icache_flush(addr: usize, size: usize);
    func codira_aligned_alloc(size: usize, align: usize) -> usize;
    func codira_aligned_free(addr: usize, size: usize, align: usize);
}

extern "codira-intrinsic" {
    func load_u64(addr: usize) -> u64;
    func store_u64(addr: usize, value: u64);
    func store_u8(addr: usize, value: u8);
}
"#;

#[test]
fn regions_allocate_reset_and_refuse_beyond_their_quota() {
    let source = format!(
        r#"{DECLARATIONS}
public func region_sum(n: usize) -> u64 {{
    let region = codira_region_new(0);
    let base = codira_region_alloc(region, n * 8, 8);
    var i = 0;
    var value: u64 = 0;
    while i < n {{
        store_u64(base + i * 8, value * 3);
        value += 1;
        i += 1;
    }}
    var total: u64 = 0;
    i = 0;
    while i < n {{
        total += load_u64(base + i * 8);
        i += 1;
    }}
    codira_region_free(region);
    total
}}

public func quota_checks(quota: usize) -> usize {{
    let region = codira_region_new(quota);
    var flags = 0;
    if codira_region_alloc(region, quota, 1) != 0 {{ flags += 1; }}
    if codira_region_alloc(region, 1, 1) == 0 {{ flags += 2; }}
    if codira_region_used(region) == quota {{ flags += 4; }}
    if codira_region_reset(region) {{ flags += 8; }}
    if codira_region_alloc(region, quota, 1) != 0 {{ flags += 16; }}
    if codira_region_free(region) {{ flags += 32; }}
    if codira_region_alloc(region, 1, 1) == 0 {{ flags += 64; }}
    if !codira_region_free(region) {{ flags += 128; }}
    flags
}}

public func aligned(align: usize) -> usize {{
    let addr = codira_aligned_alloc(100, align);
    let misalignment = addr % align;
    codira_aligned_free(addr, 100, align);
    misalignment
}}
"#
    );
    let driver = CompileAndRunTestDriver::new(&source, |builder| builder)
        .expect("Failed to build test driver");

    let n = 10_000usize;
    let sum: u64 = driver.runtime.invoke("region_sum", (n,)).unwrap();
    assert_eq!(sum, 3 * (n as u64) * (n as u64 - 1) / 2);

    let flags: usize = driver.runtime.invoke("quota_checks", (4096usize,)).unwrap();
    assert_eq!(
        flags, 255,
        "quota/reset/free behaviour (bit flags): {flags:#b}"
    );

    let misalignment: usize = driver.runtime.invoke("aligned", (4096usize,)).unwrap();
    assert_eq!(misalignment, 0);
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
#[test]
fn codira_writes_machine_code_into_a_page_and_it_runs() {
    #[cfg(target_arch = "x86_64")]
    let code: &[u8] = &[0xB8, 42, 0, 0, 0, 0xC3]; // mov eax, 42; ret
    #[cfg(target_arch = "aarch64")]
    let code: &[u8] = &[0x40, 0x05, 0x80, 0x52, 0xC0, 0x03, 0x5F, 0xD6]; // mov w0, #42; ret

    let stores: String = code
        .iter()
        .enumerate()
        .map(|(i, byte)| format!("    let b{i}: u8 = {byte};\n    store_u8(page + {i}, b{i});\n"))
        .collect();
    let source = format!(
        r#"{DECLARATIONS}
public func make_answer() -> usize {{
    let size = codira_page_size();
    // READ | WRITE
    let page = codira_page_alloc(size, 3);
    if page == 0 {{ return 0; }}
{stores}
    // READ | EXEC: never writable and executable at once
    if !codira_page_protect(page, size, 5) {{ return 0; }}
    codira_icache_flush(page, {len});
    page
}}

public func write_and_execute_is_refused() -> bool {{
    codira_page_alloc(codira_page_size(), 6) == 0
}}
"#,
        len = code.len()
    );
    let driver = CompileAndRunTestDriver::new(&source, |builder| builder)
        .expect("Failed to build test driver");

    let refused: bool = driver
        .runtime
        .invoke("write_and_execute_is_refused", ())
        .unwrap();
    assert!(refused);

    let page: usize = driver.runtime.invoke("make_answer", ()).unwrap();
    assert_ne!(page, 0, "mapping or protecting the page failed");
    // Safety: the page holds a complete function with the C ABI.
    let function: extern "C" fn() -> u32 = unsafe { std::mem::transmute(page) };
    assert_eq!(function(), 42);
    assert!(codira_runtime::native_memory::codira_page_free(
        page,
        codira_runtime::native_memory::codira_page_size()
    ));
}

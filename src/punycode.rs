//! Punycode (RFC 3492) encoder & decoder.
//!
//! 纯 Rust std 实现，零依赖、无 `unsafe`、无平台相关代码，
//! 可编译到任意 Rust 目标（含 `wasm32-unknown-unknown` / WASI），
//! 也可作为 `cdylib` 通过 C ABI 暴露给其它语言（如 Perl 的 FFI::Platypus）。
//!
//! 提供两层 API：
//! - 核心算法：[`encode_punycode`] / [`decode_punycode`]（不含 `xn--` 前缀处理）
//! - 便捷封装：[`encode`] / [`decode`]、域名分段处理 [`encode_domain`] /
//!   [`decode_domain`]，以及虚拟机名称解码 [`decode_vm_name`]
//!
//! 与 Flutter 端 `PunycodeDecoder` 行为保持一致。
//!
//! Copied from pxvdi-sdk/pxvdi-punycode (without the wasm bindings).

// ---- RFC 3492 参数 ----
const BASE: u32 = 36;
const TMIN: u32 = 1;
const TMAX: u32 = 26;
const SKEW: u32 = 38;
const DAMP: u32 = 700;
const INITIAL_BIAS: u32 = 72;
const INITIAL_N: u32 = 128;

/// 解码单个 Punycode 数字（0..36），无效字符返回 `BASE`。
fn decode_digit(byte: u8) -> u32 {
    match byte {
        b'0'..=b'9' => (byte - b'0') as u32 + 26,
        b'A'..=b'Z' => (byte - b'A') as u32,
        b'a'..=b'z' => (byte - b'a') as u32,
        _ => BASE,
    }
}

/// 把 0..36 的数字编码为对应的 Punycode 字符（小写）。
fn encode_digit(d: u32) -> u8 {
    // 0..26 -> 'a'..'z'，26..36 -> '0'..'9'
    if d < 26 {
        b'a' + d as u8
    } else {
        b'0' + (d - 26) as u8
    }
}

/// 自适应偏移量计算（RFC 3492 §6.1）。
fn adapt_bias(mut delta: u32, num_points: u32, first_time: bool) -> u32 {
    delta = if first_time { delta / DAMP } else { delta / 2 };
    delta += delta / num_points;

    let mut k = 0u32;
    while delta > ((BASE - TMIN) * TMAX) / 2 {
        delta /= BASE - TMIN;
        k += BASE;
    }

    k + (((BASE - TMIN + 1) * delta) / (delta + SKEW))
}

/// Punycode 核心解码（输入不含 `xn--` 前缀）。
///
/// 成功返回解码后的 Unicode 字符串，失败返回静态错误信息。
pub fn decode_punycode(input: &str) -> Result<String, &'static str> {
    let mut output: Vec<u32> = Vec::new();
    let mut n = INITIAL_N;
    let mut bias = INITIAL_BIAS;
    let mut i: u32 = 0;

    // 找到最后一个 '-' 分隔符
    let last_delim = input.rfind('-');

    // 基本字符（分隔符之前的部分）
    let basic_end = match last_delim {
        Some(pos) => {
            for ch in input[..pos].chars() {
                if ch as u32 >= 0x80 {
                    return Err("Invalid basic code point");
                }
                output.push(ch as u32);
            }
            pos + 1
        }
        None => 0,
    };

    // 解码扩展字符
    let mut index = basic_end;
    let bytes = input.as_bytes();

    while index < bytes.len() {
        let oldi = i;
        let mut w: u32 = 1;

        let mut k = BASE;
        loop {
            if index >= bytes.len() {
                return Err("Invalid input");
            }

            let digit = decode_digit(bytes[index]);
            index += 1;

            if digit >= BASE {
                return Err("Invalid digit");
            }

            i = i
                .checked_add(digit.checked_mul(w).ok_or("Overflow")?)
                .ok_or("Overflow")?;

            let t = if k <= bias {
                TMIN
            } else if k >= bias + TMAX {
                TMAX
            } else {
                k - bias
            };

            if digit < t {
                break;
            }

            w = w.checked_mul(BASE - t).ok_or("Overflow")?;
            k += BASE;
        }

        let out_len = (output.len() as u32) + 1;
        bias = adapt_bias(i - oldi, out_len, oldi == 0);
        n = n.checked_add(i / out_len).ok_or("Overflow")?;
        i %= out_len;

        output.insert(i as usize, n);
        i += 1;
    }

    output
        .iter()
        .map(|&cp| char::from_u32(cp).ok_or("Invalid code point"))
        .collect()
}

/// Punycode 核心编码（输出不含 `xn--` 前缀）。
///
/// 输入任意 Unicode 字符串，返回 Punycode（RFC 3492 §6.3）。
/// 纯 ASCII 输入会原样作为基本字符输出并附加一个 `-` 分隔符。
pub fn encode_punycode(input: &str) -> Result<String, &'static str> {
    let codepoints: Vec<u32> = input.chars().map(|c| c as u32).collect();
    let total = codepoints.len() as u32;

    let mut output = String::new();

    // 1. 先输出所有基本字符（< 0x80）
    let mut handled: u32 = 0;
    for &cp in &codepoints {
        if cp < 0x80 {
            output.push(cp as u8 as char);
            handled += 1;
        }
    }
    let basic_count = handled;

    // 2. 若存在基本字符，追加分隔符 '-'
    if basic_count > 0 {
        output.push('-');
    }

    let mut n = INITIAL_N;
    let mut delta: u32 = 0;
    let mut bias = INITIAL_BIAS;

    // 3. 主循环：依次编码每个非基本字符
    while handled < total {
        // 找到 >= n 的最小码点
        let m = codepoints
            .iter()
            .copied()
            .filter(|&cp| cp >= n)
            .min()
            .ok_or("No code point found")?;

        delta = delta
            .checked_add((m - n).checked_mul(handled + 1).ok_or("Overflow")?)
            .ok_or("Overflow")?;
        n = m;

        for &cp in &codepoints {
            if cp < n {
                delta = delta.checked_add(1).ok_or("Overflow")?;
            }
            if cp == n {
                // 将 delta 编码为可变长整数
                let mut q = delta;
                let mut k = BASE;
                loop {
                    let t = if k <= bias {
                        TMIN
                    } else if k >= bias + TMAX {
                        TMAX
                    } else {
                        k - bias
                    };

                    if q < t {
                        break;
                    }

                    let digit = t + ((q - t) % (BASE - t));
                    output.push(encode_digit(digit) as char);
                    q = (q - t) / (BASE - t);
                    k += BASE;
                }

                output.push(encode_digit(q) as char);
                bias = adapt_bias(delta, handled + 1, handled == basic_count);
                delta = 0;
                handled += 1;
            }
        }

        delta += 1;
        n += 1;
    }

    Ok(output)
}

/// 解码 Punycode 字符串。
///
/// 支持两种格式：
/// - 标准 IDN 格式：`xn--nxasmq5b`（以 `xn--` 开头，大小写不敏感）
/// - 纯 Punycode 格式：`nxasmq5b`
///
/// 解码失败时原样返回输入。
pub fn decode(input: &str) -> String {
    if input.is_empty() {
        return input.to_string();
    }

    let punycode = if input.to_lowercase().starts_with("xn--") {
        &input[4..]
    } else {
        input
    };

    decode_punycode(punycode).unwrap_or_else(|_| input.to_string())
}

/// 编码为标准 IDN 标签。
///
/// - 纯 ASCII 输入：原样返回（不加 `xn--` 前缀，符合 IDNA 习惯）。
/// - 含非 ASCII 字符：返回带 `xn--` 前缀的 Punycode 标签。
/// - 编码失败时原样返回输入。
pub fn encode(input: &str) -> String {
    if input.is_empty() || input.is_ascii() {
        return input.to_string();
    }

    match encode_punycode(input) {
        Ok(encoded) => format!("xn--{}", encoded),
        Err(_) => input.to_string(),
    }
}

/// 按 `.` 分段解码域名。
///
/// 只解码带 `xn--` 前缀的标签，其余标签原样保留，
/// 避免把普通 ASCII 标签（如 `example`、`com`）误当作纯 Punycode 解码。
pub fn decode_domain(domain: &str) -> String {
    if domain.is_empty() {
        return domain.to_string();
    }
    domain
        .split('.')
        .map(|label| {
            if label.to_lowercase().starts_with("xn--") {
                decode(label)
            } else {
                label.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(".")
}

/// 按 `.` 分段编码域名，每个含非 ASCII 的标签转为 `xn--` 形式。
pub fn encode_domain(domain: &str) -> String {
    if domain.is_empty() {
        return domain.to_string();
    }
    domain
        .split('.')
        .map(encode)
        .collect::<Vec<_>>()
        .join(".")
}

/// 尝试解码虚拟机名称。
///
/// 只在包含明确的 `xn--` 前缀时才进行 Punycode 解码，
/// 避免将普通名称（如 `Windows10-ltsc`、`dev-win-10`）误判为 Punycode。
///
/// VM 名常以 `-` 拼接前缀（如 `Copy-of-VM-xn--xxx`），因此每个 `.` 分段内
/// 还会再定位 `xn--`：之前的 ASCII 前缀原样保留，从 `xn--` 起到段末作为
/// 一个完整 Punycode 标签解码（与 IDN 标签语义一致，中间的 `-` 属于 payload）。
pub fn decode_vm_name(name: &str) -> String {
    if name.is_empty() {
        return name.to_string();
    }

    if !name.to_lowercase().contains("xn--") {
        return name.to_string();
    }

    name.split('.')
        .map(|part| {
            let lower = part.to_lowercase();
            // 在段内查找 xn-- 起始位置：要么在段首，要么紧跟一个 '-'
            let pos = if lower.starts_with("xn--") {
                Some(0)
            } else {
                lower.find("-xn--").map(|p| p + 1)
            };

            match pos {
                Some(p) => {
                    let prefix = &part[..p];
                    let label = &part[p..];
                    format!("{}{}", prefix, decode(label))
                }
                None => part.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join(".")
}


#[cfg(test)]
mod tests {
    use super::*;

    // ---- 解码 ----
    #[test]
    fn test_decode_xn_prefix() {
        assert_eq!(decode("xn--fiq228c"), "中文");
    }

    #[test]
    fn test_decode_empty() {
        assert_eq!(decode(""), "");
    }

    #[test]
    fn test_decode_pure_punycode() {
        assert_eq!(decode("fiq228c"), "中文");
    }

    // ---- 编码 ----
    #[test]
    fn test_encode_chinese() {
        assert_eq!(encode("中文"), "xn--fiq228c");
    }

    #[test]
    fn test_encode_pure_ascii_passthrough() {
        // 纯 ASCII 不加前缀
        assert_eq!(encode("hello"), "hello");
        assert_eq!(encode("dev-win-10"), "dev-win-10");
    }

    #[test]
    fn test_encode_empty() {
        assert_eq!(encode(""), "");
    }

    #[test]
    fn test_encode_punycode_raw_chinese() {
        // 不含前缀的核心编码
        assert_eq!(encode_punycode("中文").unwrap(), "fiq228c");
    }

    // ---- 往返 ----
    #[test]
    fn test_round_trip() {
        for s in ["中文", "日本語", "münchen", "bücher", "ascii-only", "café", "🦀rust"] {
            let encoded = encode(s);
            let decoded = decode(&encoded);
            assert_eq!(decoded, s, "round-trip failed for {:?} -> {:?}", s, encoded);
        }
    }

    #[test]
    fn test_round_trip_hyphen_mixed() {
        // ASCII 与非 ASCII 用连字符交错：分隔符处理的关键用例
        for s in [
            "中文-x11-中文",
            "x11-中文-中文",
            "中文-中文",
            "-中文",
            "中文-",
            "a-中-b-文-c",
            "x11-中文",
            "中文-x11",
        ] {
            let encoded = encode(s);
            let decoded = decode(&encoded);
            assert_eq!(decoded, s, "round-trip failed for {:?} -> {:?}", s, encoded);
        }
    }

    #[test]
    fn test_round_trip_mixed_label() {
        // RFC 3492 经典例子：含基本字符 + 非基本字符
        let encoded = encode_punycode("bücher").unwrap();
        assert_eq!(decode_punycode(&encoded).unwrap(), "bücher");
    }

    // ---- 域名分段 ----
    #[test]
    fn test_domain_round_trip() {
        let domain = "中文.example.com";
        let encoded = encode_domain(domain);
        assert_eq!(encoded, "xn--fiq228c.example.com");
        assert_eq!(decode_domain(&encoded), domain);
    }

    // ---- VM 名称 ----
    #[test]
    fn test_decode_vm_name_with_xn() {
        assert_eq!(decode_vm_name("xn--fiq228c"), "中文");
    }

    #[test]
    fn test_decode_vm_name_mixed() {
        assert_eq!(decode_vm_name("xn--fiq228c.normal"), "中文.normal");
    }

    #[test]
    fn test_decode_vm_name_no_xn() {
        assert_eq!(decode_vm_name("Windows10-ltsc"), "Windows10-ltsc");
        assert_eq!(decode_vm_name("my-vm-01"), "my-vm-01");
    }

    #[test]
    fn test_decode_vm_name_xn_after_hyphen_prefix() {
        // VM 名常见形态：ASCII 前缀 + '-' + xn-- 标签
        assert_eq!(
            decode_vm_name("Copy-of-VM-xn--winssssws--ys5qt78h7rtmv2f"),
            "Copy-of-VM-winssss是ws-开发组"
        );
        assert_eq!(decode_vm_name("prefix-xn--fiq228c"), "prefix-中文");
        // 不应误判：xn-- 不在词边界（无前导 '-' 也不在段首）
        assert_eq!(decode_vm_name("notxn--fiq228c"), "notxn--fiq228c");
    }

    #[test]
    fn test_decode_vm_name_empty() {
        assert_eq!(decode_vm_name(""), "");
    }
}

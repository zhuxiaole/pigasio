//! ASIO 通道名的生成。
//!
//! # 为什么需要单独一个模块
//!
//! 通道名的唯一用途是让用户在宿主的通道列表里认出「这一路是哪块声卡的
//! 第几个口」。听起来简单,但有两个约束会互相打架:
//!
//! 1. ASIO 只给通道名 **32 字节**(含结尾的 0),而 Windows 的设备名往往
//!    很长,还常带一长串括号说明 —— "扬声器 (Realtek(R) Audio)"。
//! 2. 用户机器上经常有**多块名字前缀相同的设备**。这台测试机上就有四个
//!    以 "扬声器" 开头的设备,区别全在括号里:
//!
//!    ```text
//!    扬声器 (KO-STAR M-640 )
//!    扬声器 (Realtek(R) Audio)
//!    扬声器 (AB13X USB Audio)
//!    扬声器 (MG-10)
//!    ```
//!
//! 早先的实现为了塞进 32 字节,粗暴地丢掉括号里的内容,结果这四块设备
//! 的通道名全都成了 `OUT 1 (扬声器)` —— 在机架里根本分不清哪路是哪路。
//!
//! 现在的做法是分级降级:先用最短的形式,**只在发生重名时**才逐步补充
//! 区分信息。这样常见的"设备名本来就不一样"的场景依然简洁,而重名场景
//! 会自动带上能区分的部分。

use std::collections::HashMap;

use crate::config::Config;
use crate::engine::StreamInfo;
use crate::error::StreamKind;

/// 通道名字段的总字节预算。
///
/// ASIO 给的是 `char name[32]`,协议要求以 0 结尾,所以实际可用 31 字节。
/// 这是 1996 年定下的硬限制,任何 ASIO 驱动都绕不过去。
const NAME_BUDGET: usize = 31;

/// 标签部分的字节预算(即去掉 `OUT 12 (` 和 `)` 之后剩下多少)。
///
/// 前缀最坏情况是双位通道号:`OUT 12 (` 是 8 字节,加收尾的 `)` 共 9 字节。
/// 31 - 9 = 22。
///
/// 早先的实现按**字符数**限制(18 个字符),那对纯 ASCII 名字是浪费
/// —— 18 个字符只占 18 字节,明明还能再放 4 个;对中文名字又可能超,
/// 因为一个汉字在 GBK 下是 2 字节,18 个汉字要 36 字节。按字节算才对得上
/// ASIO 真正的约束。
const LABEL_BUDGET: usize = NAME_BUDGET - 9;

/// 估算一个字符在系统 ANSI 代码页下占几个字节。
///
/// ASCII 是单字节;中文、日文、韩文在各自的 ANSI 代码页(GBK、Shift-JIS、
/// UHC)里都是双字节。这里只需要**不低估**:低估会让最终写入超长,而
/// `set_name` 那层的兜底截断可能把一个双字节字符切成两半,直接变乱码。
fn ansi_len(c: char) -> usize {
    if c.is_ascii() {
        1
    } else {
        2
    }
}

/// 按 ANSI 字节预算截断,不会切断字符。
fn truncate_to_bytes(s: &str, max_bytes: usize) -> String {
    let mut used = 0usize;
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let n = ansi_len(c);
        if used + n > max_bytes {
            break;
        }
        used += n;
        out.push(c);
    }
    out
}

/// 整个字符串在 ANSI 代码页下占多少字节。
fn ansi_bytes(s: &str) -> usize {
    s.chars().map(ansi_len).sum()
}

/// 某个方向上每个 ASIO 通道的显示名,按通道号索引。
#[derive(Debug, Clone, Default)]
pub struct ChannelNames {
    inputs: Vec<String>,
    outputs: Vec<String>,
}

impl ChannelNames {
    /// 按配置和设备列表生成全部通道名。
    ///
    /// `streams` 是 [`crate::Engine::stream_infos`] 的内容:先输入后输出,
    /// 每个方向的设备内部按 ASIO 通道偏移排列。
    pub fn build(config: &Config, streams: &[StreamInfo]) -> Self {
        let allow_non_ascii = config.engine.use_non_ascii_channel_names;
        ChannelNames {
            inputs: build_for(StreamKind::Input, streams, allow_non_ascii),
            outputs: build_for(StreamKind::Output, streams, allow_non_ascii),
        }
    }

    /// 取某个通道的显示名。
    pub fn get(&self, kind: StreamKind, channel: usize) -> Option<&str> {
        let list = match kind {
            StreamKind::Input => &self.inputs,
            StreamKind::Output => &self.outputs,
        };
        list.get(channel).map(String::as_str)
    }

    pub fn inputs(&self) -> &[String] {
        &self.inputs
    }

    pub fn outputs(&self) -> &[String] {
        &self.outputs
    }

    /// 这个方向上所有通道名是否两两不同。
    ///
    /// 给测试和诊断用:重名在机架里是致命的,值得能直接断言。
    pub fn all_unique(&self, kind: StreamKind) -> bool {
        let list = match kind {
            StreamKind::Input => &self.inputs,
            StreamKind::Output => &self.outputs,
        };
        let unique: std::collections::HashSet<&String> = list.iter().collect();
        unique.len() == list.len()
    }
}

/// 生成某个方向上所有通道的名字。
fn build_for(kind: StreamKind, streams: &[StreamInfo], allow_non_ascii: bool) -> Vec<String> {
    let devices: Vec<&StreamInfo> = streams.iter().filter(|s| s.kind == kind).collect();
    if devices.is_empty() {
        return Vec::new();
    }

    let labels = labels_for(&devices, allow_non_ascii);
    let prefix = if kind == StreamKind::Input {
        "IN"
    } else {
        "OUT"
    };

    // 把每个设备的标签展开成它占用的那些通道。
    //
    // 依赖 `StreamInfo::asio_channel_offset` 是从 0 开始连续编号的 ——
    // 引擎构造时保证了这一点。
    let mut names = Vec::new();
    for (device, label) in devices.iter().zip(labels.iter()) {
        for local in 0..device.channel_count {
            let name = format!("{prefix} {} ({label})", local + 1);
            // 开发期保险:名字必须放得进 ASIO 的字节预算。真超了会被
            // `set_name` 那层截断,而那里可能把一个汉字切成两半 ——
            // 与其让用户在机架里看到乱码,不如在测试里就炸出来。
            debug_assert!(
                ansi_bytes(&name) <= NAME_BUDGET,
                "通道名 “{name}” 占 {} 字节,超过 ASIO 的 {NAME_BUDGET} 字节上限",
                ansi_bytes(&name)
            );
            names.push(name);
        }
    }
    names
}

/// 为每个设备算一个标签,并保证互不相同。
fn labels_for(devices: &[&StreamInfo], allow_non_ascii: bool) -> Vec<String> {
    if !allow_non_ascii {
        // 纯 ASCII 模式:用设备序号。设备名可能是纯中文,剔掉非 ASCII
        // 会得到空标签,不如直接用序号 —— 它同样能说明"来自哪块声卡"。
        return (0..devices.len())
            .map(|i| format!("dev{}", i + 1))
            .collect();
    }

    // 第一级:最短形式(括号之前的部分)。
    let mut labels: Vec<String> = devices
        .iter()
        .map(|d| truncate_to_bytes(short_label(&d.device_name), LABEL_BUDGET))
        .collect();

    // 第二级:重名的补上括号里的关键词(通常是厂商或型号)。
    if has_duplicates(&labels) {
        let totals = counts(&labels);
        for (i, device) in devices.iter().enumerate() {
            if totals.get(labels[i].as_str()).copied().unwrap_or(0) > 1 {
                labels[i] = truncate_to_bytes(&long_label(&device.device_name), LABEL_BUDGET);
            }
        }
    }

    // 第三级:补了关键词还是重名(同型号买了两块),加序号。
    if has_duplicates(&labels) {
        let totals = counts(&labels);
        let mut seen: HashMap<String, usize> = HashMap::new();
        for label in labels.iter_mut() {
            if totals.get(label.as_str()).copied().unwrap_or(0) > 1 {
                let nth = {
                    let n = seen.entry(label.clone()).or_insert(0);
                    *n += 1;
                    *n
                };
                // 先给 " #N" 留出位置再截断。直接拼上序号再截断的话,
                // 序号本身会被截掉,两个标签还是长得一模一样 ——
                // 这个坑是测试逼出来的。
                let base = truncate_to_bytes(label.as_str(), LABEL_BUDGET.saturating_sub(3));
                *label = format!("{base} #{nth}");
            }
        }
    }

    labels
}

fn counts(labels: &[String]) -> HashMap<String, usize> {
    let mut map = HashMap::new();
    for l in labels {
        *map.entry(l.clone()).or_insert(0) += 1;
    }
    map
}

fn has_duplicates(labels: &[String]) -> bool {
    let unique: std::collections::HashSet<&String> = labels.iter().collect();
    unique.len() != labels.len()
}

/// 最短形式:括号之前的部分。
///
/// `扬声器 (Realtek(R) Audio)` → `扬声器`
fn short_label(name: &str) -> &str {
    let base = match name.find('(') {
        Some(idx) => name[..idx].trim(),
        None => name.trim(),
    };
    // 整串都以括号开头时(少见但可能),退回原名,免得得到空标签。
    if base.is_empty() {
        name.trim()
    } else {
        base
    }
}

/// 加长形式:简称 + 括号里的完整内容。
///
/// `扬声器 (MG-10)`           → `扬声器 MG-10`
/// `扬声器 (AB13X USB Audio)` → `扬声器 AB13X USB Audio`
///
/// 超预算的部分交给 [`truncate_to_bytes`] 处理。
///
/// 早先这里只取括号里的**第一个词**,结果 `MG-10` 变成 `MG`、
/// `AB13X USB Audio` 变成 `AB13X` —— 丢掉的恰恰是区分设备的关键部分,
/// 而字节预算明明还有富余。既然 31 字节是上限而不是目标,就该尽量带全。
fn long_label(name: &str) -> String {
    let short = short_label(name);
    let Some(open) = name.find('(') else {
        return short.to_string();
    };
    let inner = &name[open + 1..];
    // 有些设备名里还套着括号,比如 "Realtek(R) Audio",先切到第一层结束。
    let inner = inner.split(')').next().unwrap_or(inner).trim();

    if inner.is_empty() {
        short.to_string()
    } else {
        format!("{short} {inner}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::StreamInfo;

    fn device(kind: StreamKind, name: &str, offset: usize, channels: usize) -> StreamInfo {
        StreamInfo {
            kind,
            device_name: name.to_string(),
            is_clock_master: false,
            asio_channel_offset: offset,
            channel_count: channels,
            device_channel_map: (0..channels).collect(),
        }
    }

    fn config_with_non_ascii(allow: bool) -> Config {
        let mut config = Config::default();
        config.engine.use_non_ascii_channel_names = allow;
        config
    }

    #[test]
    fn 不同设备名的通道名各不相同() {
        let streams = vec![
            device(StreamKind::Output, "Speakers (Realtek)", 0, 2),
            device(StreamKind::Output, "S/PDIF (Realtek)", 2, 2),
        ];
        let names = ChannelNames::build(&config_with_non_ascii(true), &streams);
        assert_eq!(
            names.outputs(),
            [
                "OUT 1 (Speakers)",
                "OUT 2 (Speakers)",
                "OUT 1 (S/PDIF)",
                "OUT 2 (S/PDIF)"
            ]
        );
        assert!(names.all_unique(StreamKind::Output));
    }

    /// 这是 "机架里两个通道名一样" 那个 bug 的回归测试。
    ///
    /// 测试机上有四块都以 "扬声器" 开头的设备,区别全在括号里。
    /// 早先的实现丢掉括号,于是四块设备的标签全都是 "扬声器"。
    #[test]
    fn 同名前缀的设备名不会撞车() {
        let streams = vec![
            device(StreamKind::Output, "扬声器 (KO-STAR M-640 )", 0, 2),
            device(StreamKind::Output, "扬声器 (Realtek(R) Audio)", 2, 2),
            device(StreamKind::Output, "扬声器 (AB13X USB Audio)", 4, 2),
            device(StreamKind::Output, "扬声器 (MG-10)", 6, 2),
        ];
        let names = ChannelNames::build(&config_with_non_ascii(true), &streams);

        assert!(
            names.all_unique(StreamKind::Output),
            "通道名有重复,机架里无法区分:{:?}",
            names.outputs()
        );
        // 而且不能退化成一堆 dev1/dev2 —— 那等于没解决问题。
        assert!(
            names.outputs().iter().all(|n| n.contains("扬声器")),
            "丢了设备名:{:?}",
            names.outputs()
        );
        // 关键词应当出现在名字里,用户才认得出。
        assert!(
            names.outputs()[2].contains("Realtek"),
            "{:?}",
            names.outputs()
        );
        assert!(
            names.outputs()[4].contains("AB13X"),
            "{:?}",
            names.outputs()
        );
    }

    #[test]
    fn 完全同名的设备会加序号区分() {
        let streams = vec![
            device(StreamKind::Output, "USB Audio Device", 0, 1),
            device(StreamKind::Output, "USB Audio Device", 1, 1),
        ];
        let names = ChannelNames::build(&config_with_non_ascii(true), &streams);
        assert!(names.all_unique(StreamKind::Output));
        assert!(names.outputs()[0].contains("#1"), "{:?}", names.outputs());
        assert!(names.outputs()[1].contains("#2"), "{:?}", names.outputs());
    }

    #[test]
    fn ascii_模式只用设备序号() {
        let streams = vec![
            device(StreamKind::Output, "扬声器 (Realtek)", 0, 2),
            device(StreamKind::Output, "扬声器 (MG-10)", 2, 2),
        ];
        let names = ChannelNames::build(&config_with_non_ascii(false), &streams);
        assert_eq!(
            names.outputs(),
            [
                "OUT 1 (dev1)",
                "OUT 2 (dev1)",
                "OUT 1 (dev2)",
                "OUT 2 (dev2)"
            ]
        );
        assert!(names.outputs().iter().all(|n| n.is_ascii()));
        assert!(names.all_unique(StreamKind::Output));
    }

    #[test]
    fn 输入输出各自独立编号且前缀不同() {
        let streams = vec![
            device(StreamKind::Input, "Mic (USB)", 0, 1),
            device(StreamKind::Output, "Speakers (USB)", 0, 2),
        ];
        let names = ChannelNames::build(&config_with_non_ascii(true), &streams);
        assert_eq!(names.inputs(), ["IN 1 (Mic)"]);
        assert_eq!(names.outputs(), ["OUT 1 (Speakers)", "OUT 2 (Speakers)"]);
    }

    #[test]
    fn 没有括号的设备名原样保留() {
        let streams = vec![device(StreamKind::Output, "Loopback", 0, 1)];
        let names = ChannelNames::build(&config_with_non_ascii(true), &streams);
        assert_eq!(names.outputs(), ["OUT 1 (Loopback)"]);
    }

    #[test]
    fn 超长设备名会按字节预算截断() {
        let long = format!("Very Long Device Name That Goes On {}", "x".repeat(80));
        let streams = vec![device(StreamKind::Output, &long, 0, 1)];
        let names = ChannelNames::build(&config_with_non_ascii(true), &streams);
        let name = &names.outputs()[0];
        // 整个名字必须放得进 ASIO 的 31 字节预算 —— 这是硬约束,
        // 超了就会被 `set_name` 那层截断,而那里可能切碎一个汉字。
        let used = ansi_bytes(name);
        assert!(used <= NAME_BUDGET, "名字占 {used} 字节,超过 {NAME_BUDGET}");
    }

    #[test]
    fn 纯_ascii_名字用满字节预算() {
        // 22 个 ASCII 字符 = 22 字节,正好是标签的全部预算。
        // 早先按"字符数"限制成 18,白白浪费了 4 个字符的位置。
        let name = "ABCDEFGHIJKLMNOPQRSTUVWXYZ";
        let streams = vec![device(StreamKind::Output, name, 0, 1)];
        let names = ChannelNames::build(&config_with_non_ascii(true), &streams);
        let got = &names.outputs()[0];
        assert_eq!(
            got.as_str(),
            format!("OUT 1 ({})", &name[..LABEL_BUDGET]).as_str(),
            "ASCII 名字没有用满预算"
        );
        assert_eq!(ansi_bytes(got), NAME_BUDGET - 9 + 8);
    }

    #[test]
    fn 中文名字不会超出字节预算() {
        // 全是汉字,每个 2 字节。要确认截断发生在字符边界上,
        // 而且总长不超预算。
        let name = "这是一个非常非常长的中文设备名称用来测试截断行为";
        let streams = vec![device(StreamKind::Output, name, 0, 1)];
        let names = ChannelNames::build(&config_with_non_ascii(true), &streams);
        let got = &names.outputs()[0];
        let used = ansi_bytes(got);
        assert!(used <= NAME_BUDGET, "占 {used} 字节,超过 {NAME_BUDGET}");
        // 标签部分应当是偶数个字节(每个汉字 2 字节),说明没被切半。
        let label = got.trim_start_matches("OUT 1 (").trim_end_matches(')');
        assert_eq!(ansi_bytes(label) % 2, 0, "汉字被切成了两半:{label}");
    }

    #[test]
    fn 取不存在的通道返回_none() {
        let streams = vec![device(StreamKind::Output, "Speakers", 0, 2)];
        let names = ChannelNames::build(&config_with_non_ascii(true), &streams);
        assert!(names.get(StreamKind::Output, 0).is_some());
        assert!(names.get(StreamKind::Output, 1).is_some());
        assert!(names.get(StreamKind::Output, 2).is_none());
        assert!(names.get(StreamKind::Input, 0).is_none());
    }
}

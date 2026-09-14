# 示例配置

这两个文件可以直接用 `--config` 传给命令行工具,也可以拷到宿主目录
或用户目录后改名为 `PigASIO.toml`。

## 多设备输出示例.toml

两块输出设备 + 一块输入设备,跨两块不同的声卡。用来验证多设备同步:

```bash
pigasio check --config examples/多设备输出示例.toml --seconds 10
```

设备名是这台机器上实际存在的虚拟声卡,换到别的机器上需要改成本机有的
设备(`pigasio devices` 可以列出)。

## 虚拟声卡回环自检.toml

输出和输入指向同一根虚拟线缆的两端,配合 `--tone` 可以验证整条音频
数据通路:

```bash
pigasio check --config examples/虚拟声卡回环自检.toml --tone --seconds 8
```

如果报告里的「输入信号」峰值接近 0.25(测试音的幅度),说明
「ASIO 输出 → 引擎 → 设备 → 采集 → ASIO 输入」整条链路是通的。

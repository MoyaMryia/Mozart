# UL-UNAS 可选推理进程

`mozart-enhance` 使用 ONNX Runtime 的 CPU 执行提供程序。帧内与帧间线程数均为 1。
构建打开 `MOZART_WITH_ORT`，通过 `ONNX_RUNTIME_PATH`、`ONNX_RUNTIME_INCLUDE_DIR` 指定既有库与头文件。
关闭 ORT 时仍能构建程序及单测；程序启动明确报错退出，不提供伪增强或 identity 开关。
模型只读打开，不复制权重，不依赖 PyTorch 或 FFTW。
模型接口来自[作者 UL-UNAS 仓库](https://github.com/Xiaobin-Rong/ul-unas)，作者仓库采用 MIT 许可；权重不纳入本项目分发。

```sh
mozart-enhance --socket /run/mozart/enhance.sock \
  --audio-socket /run/mozart/enhance-audio.sock --model /assets/model.onnx
```

## 控制面

Unix `SOCK_SEQPACKET` 使用 `core/include/mozart/ctl.h` 的定长头及 JSON 信封。
Hello（`0x0701`）、GetStatus（`0x0702`）请求体均为 `{}`。
响应回显命令号和 request_id，响应体含 `status`、`code`、`reason`、`data`。
Hello 的 `data.version` 为程序版本。
GetStatus 的 `data` 包含：

- `version`、`backend:"ulunas"`。
- `frames_in`、`frames_out`：已接受的合法请求数、成功发送的 ready 应答数。
- `sessions`：已接受的音频连接数；帧序号缺口只重置该连接内状态，不增加连接数。
- `algorithm_delay_samples:640`：算法积攒的 16 kHz 样本数。
- `last_error`：初始为 null；记录最近坏包、容量拒绝或背压原因，连接重建不清除。

控制连接均为非阻塞，最多同时保留 16 个连接。空闲 30 s 后回收连接；达到容量时记录 `last_error`。

## 音频面

请求与应答直接使用 `mozart_enhance_request_t`、`mozart_enhance_reply_t`。
每个 SEQPACKET 消息恰好一包。入口检查尺寸、magic 及全部 PCM 为有限值且位于 `[-1,1]`。
坏包关闭连接，并在状态中保留原因。

同一时刻接受一个音频连接。每个新连接建立新的状态 epoch。
首两次请求应答 `ready=0`；第三次起每次应答 `ready=1`，输出两帧前源请求的 320 个样本。
ready=0 时 frame 为零，不代表增强后的音频。
源 meta 逐字节复制。客户端重算 energy 与 VAD。
`frame_idx` 不连续时清除模型缓存、STFT、OLA 和待输出元数据，重新积攒两帧。
断连同样清空状态。

应答非阻塞发送。背压立即关闭当前音频连接，`last_error` 可查询原因。
推理或合成内部异常退出进程，由 daemon 记录退出并退避重启，不返回未增强数据冒充成功。
每个监听路径持有 `<socket>.lock` 独占锁；活动 socket 不被替换。
正常退出清除本实例 socket，锁文件保留以避免锁 inode 竞争。

## 流式核

512 点 radix-2 FFT，周期 Hann 分析与合成，256 样本 hop，按窗平方和归一化。
首部反射只使用已接收样本；不读取整段录音或未来片段。
音频核只用固定大小数组，按样本推进 STFT 与 OLA。
模型输入为 257 个交错复数频点及三个连续缓存，缓存元素数为 5358、402、1056。
模型边界检查输出形状、类型及缓存有限性；流式核检查频谱及合成输出有限性。
ORT 在普通服务线程运行，可分配张量，不在采集或播放回调内运行。
`SpectrumModel` 是测试注入接口；生产入口只构造 ORT 模型。

## 离线对拍

```sh
mozart-enhance --model /assets/model.onnx \
  --input-f32 input.f32 --output-f32 output.f32
```

输入输出为本机字节序单声道 float32、16 kHz。
离线入口逐帧读取，与在线共用 `Session`，末尾补两帧零并裁回原样本数。
不足 320 样本的尾帧先补零。不加载整个录音。
打开输出前拒绝与输入或模型同 inode 的路径，包括硬链接和符号链接。

## 测试

`enhance/CMakeLists.txt` 注册 `enhance_stream`、`enhance_process`。

- 流式测试：独立 DFT 对照、随机噪声恒等重建、首尾脉冲、生产两帧排空、短文件与非 hop 对齐、元数据延迟、缺口与显式重置、输出限幅和非有限频谱。
- 进程测试：测试内注入 identity，不改变生产 CLI；覆盖 16 个满槽控制连接真实空闲 30 s 回收、容量拒绝原因、重复实例、请求尺寸／magic／PCM 边界、断连重建、背压关闭与状态查询。
- 离线保护测试：使用微小虚拟模型文件，拒绝原路径、硬链接和符号链接输出；逐字节确认模型不变。

本机已通过 `host-debug` 两项测试；开启 ORT 的 `host-asan` 同样通过两项测试。
既有真实模型已完成 float32 离线推理，输出长度与输入相同。
板端集成结果见 `reports/ulunas-integration-20261117/RESULTS.md`。模型由配置只读引用，不随源码同步。

# outliner

`outliner` 是一个给 PDF 自动补全书签目录的命令行工具. 它会先对候选页做采样, 优先用 PDF 内嵌文本让 LLM 判断目录位置. 如果文本不足, 再尝试 OCR 文本推断. 只有前两步都不可靠时, 才回退到页面图片判断和目录提取, 最后把结果写回 PDF outline.

适合这类输入:

- PDF 里已经有清晰的目录页, 但没有书签
- 目录页的页码是印刷页码, 不是 PDF 物理页码
- 需要保留原 PDF 内容, 只补充 outline

不适合这类输入:

- PDF 没有目录页
- 目录页本身严重模糊, 裁切错误, 或页码不可见
- 需要识别整本书结构, 而不是基于现成目录页生成书签

## 快速上手

### 1. 准备依赖

项目依赖以下本地工具:

- Rust
- `pdftoppm`, 用于把 PDF 页面渲染成 PNG
- `pdftotext`, 用于提取指定 PDF 页面里的内嵌文字
- `tesseract`, 用于在没有可用内嵌文字时做 OCR 兜底

程序还会调用兼容 OpenAI Responses/Chat Completions 风格的视觉模型接口. 默认读取:

- `OPENAI_API_KEY`
- `OPENAI_BASE_URL`, 可选

也可以使用配置文件 `~/.config/outliner/config.toml`.

### 2. 构建

```bash
cargo build
```

### 3. 准备配置

最小配置如下:

```toml
model = "gpt-4o-mini"
api_base = "https://api.openai.com/v1"
api_key = "sk-..."
vision_worker_batch_size = 4
vision_workers = 4
```

如果不写配置文件, 也可以只设置环境变量:

```bash
export OPENAI_API_KEY="sk-..."
```

### 4. 运行

```bash
outliner ./assets/sample-text.pdf
```

默认会在输入文件同目录生成一个新文件:

```text
outlined_sample-text.pdf
```

指定输出路径:

```bash
outliner ./book.pdf --output ./book.outlined.pdf
```

按批次并行处理图片:

```bash
outliner ./book.pdf --vision-worker-batch-size 2 --vision-workers 4
```

限定目录页搜索范围:

```bash
outliner ./book.pdf --toc 3..9
```

`--toc` 支持三种形式:

- `3..9`: 只在第 3 到第 9 个 PDF 物理页里处理目录
- `3..`: 从第 3 页开始处理到文末
- `..9`: 从第 1 页处理到第 9 页

查看帮助:

```bash
outliner --help
```

## 命令行参数

```text
outliner [OPTIONS] <INPUT>
```

- `<INPUT>`: 输入 PDF 路径
- `--output <OUTPUT>`: 输出 PDF 路径. 不传时默认生成 `outlined_<原文件名>.pdf`
- `--model <MODEL>`: 覆盖配置文件中的模型名
- `--config <PATH>`: 指定配置文件路径, 默认是 `~/.config/outliner/config.toml`
- `--toc <RANGE>`: 指定目录页搜索或处理范围
- `--vision-worker-batch-size <N>`: 每个 worker 每批读取多少张图片, 默认 `4`
- `--vision-workers <N>`: worker 并发数量, 默认 `4`

## 项目原理

整个流程分成 3 层.

### 1. PDF 访问层

- [`src/qpdf_outline.rs`](/Users/azazo1/pjs/rust/outliner/src/qpdf_outline.rs) 负责打开 PDF, 读取已有 outline, 以及把新的 outline 树写回去
- [`src/pdf_support.rs`](/Users/azazo1/pjs/rust/outliner/src/pdf_support.rs) 负责页面采样、页面渲染、页码观测样本选择, 以及把目录中的印刷页码映射回 PDF 物理页

这里的关键点是区分两种页码:

- PDF 物理页码: 文件里的第几页, 从 1 开始
- 印刷页码: 页面上真正印出来的页码, 可能从罗马数字开始, 也可能正文从 1 重新计数

### 目录发现方法

目录自动发现不再只依赖图片. 当前方法按下面的顺序工作:

1. 先确定候选区间. 如果没有传完整的 `--toc a..b`, 默认只在文档前部搜索.
2. 每轮从候选区间里固定采样 12 页:
   - 前 8 个样本取候选区间前 16 页中的单数页, 即相对位置 `1, 3, 5, 7, 9, 11, 13, 15`
   - 后 4 个样本从候选区间前 30% 中, 对 16 页之后的部分做均匀分布
   - 如果前 30% 太短, 则从候选区间起始处继续补齐, 直到满 12 页
3. 先对这些采样页执行 `pdftotext`, 让 LLM 仅根据文字判断:
   - `hit`: 当前页就是目录页
   - `after`: 目录更可能在当前页之后, 常见于封面, 扉页, 版权页, 出版信息页, 前言等
   - `before`: 目录更可能在当前页之前, 常见于正文, 附录, 参考文献, 索引等
   - `unknown`: 文字不足, 无法可靠判断
4. 如果内嵌文字无法给出可用结果, 再对同一批页面做 OCR, 然后重复同样的方向判断.
5. 如果 OCR 之后仍然不能定位, 再把这些页面渲染成图片, 让多模态模型根据版面和页面内容做最后一轮判断.
6. 只要出现 `hit`, 程序就围绕命中的页收缩目录区间. 如果没有 `hit`, 就利用 `before` 和 `after` 缩小搜索窗口.
7. 当缩小后的候选区间已经足够短时, 程序直接渲染整个区间作为最终目录候选页.

这样做的目的, 是优先利用 PDF 里更稳定的文字信息, 其次利用 OCR, 最后才依赖版面视觉判断. 对目录很靠前, 但整本 PDF 很长的文档, 这种分层策略比直接均匀采样图片更稳.

### 2. LLM 推断层

- [`src/llm.rs`](/Users/azazo1/pjs/rust/outliner/src/llm.rs) 负责两类请求:
  - 基于页面文字的 TOC 定位判断
  - 基于页面图片的 TOC 定位, 目录提取和正文页码观测
- 目录定位时, 模型不会只回答"像不像目录", 而是必须对每个采样页返回 `hit`, `before`, `after`, `unknown` 之一
- 目录提取阶段, 模型负责从最终目录页里提取条目, 层级和印刷页码
- 页码标定阶段, 模型负责读取正文样本页上真实可见的印刷页码

图片不会默认一次性全部送进一个请求. 程序会先按 `vision_worker_batch_size` 分批, 再按 `vision_workers` 并行执行. 每个 worker 都会显示独立的子进度条.

程序并不让模型直接猜测整本书结构, 而是让模型做 4 个更窄的任务:

1. 从内嵌文本推断目录方向
2. 必要时从 OCR 或图片推断目录方向
3. 抄目录条目
4. 读实际页码

这样做的目的, 是把模型自由发挥的空间压小, 尽量把错误限制在 OCR 和版面识别层面.

### 3. 结构归一化层

- [`src/model.rs`](/Users/azazo1/pjs/rust/outliner/src/model.rs) 负责目录条目标准化, 标题清洗, 罗马数字页码解析, 以及 outline 对比所需的数据结构
- [`src/main.rs`](/Users/azazo1/pjs/rust/outliner/src/main.rs) 把各阶段串起来, 并在写回前判断现有书签是否已经和目标目录一致

这里主要解决 3 个问题:

- LLM 返回的层级可能跳级, 需要压平成连续层级
- 目录里的标题和已有 outline 标题可能只在空白或符号上不同, 对比前需要归一化
- 目录里的页码是印刷页码, 需要先根据样本页推断 offset, 再换算成物理页码

## 完整执行流程

一次典型运行的顺序如下:

1. 打开输入 PDF, 读取总页数
2. 构建 `PdfWorkspace`, 计算目录搜索范围
3. 如果 `--toc` 没有给出完整闭区间, 先在候选区间中采样 12 页. 其中前 8 个样本固定覆盖前 16 页的单数页, 后 4 个样本覆盖候选区间前 30% 的后半段
4. 对采样页执行 `pdftotext`, 让 LLM 先根据文字判断目录命中页或方向
5. 如果内嵌文字不足, 对同一批页执行 OCR, 再让 LLM 判断一次
6. 如果 OCR 仍不足以定位, 再把采样页渲染成 PNG, 交给多模态 LLM 判断目录命中页或方向
7. 根据 `hit` 或 `before/after` 提示缩小目录候选范围, 必要时重复采样和定位
8. 渲染候选目录页, 让视觉 LLM 提取目录条目, 层级和印刷页码
9. 规范化目录条目, 必要时补一个 "目录" 顶级节点
10. 根据目录里出现的页码范围, 在正文区域再采样一些页面
11. 渲染这些正文页, 让视觉 LLM 读取页面上真实可见的印刷页码
12. 用观测到的页码样本推断页码偏移量, 把目录条目换算成 PDF 物理页
13. 读取 PDF 现有 outline, 做归一化对比
14. 如果已有 outline 已经一致, 则停止, 不写输出
15. 如果不一致, 使用 `qpdf` 写入新的 outline
16. 输出最终状态, 包括条目数, agent 调用次数和 token 用量

当 `--toc 3..9` 这种完整区间已经给出时, 流程会跳过目录页自动发现阶段, 直接处理指定范围.

## 输出行为

程序有 3 种结束结果:

- `NoTocFound`: 没找到可信目录, 或提取出的目录条目不足
- `AlreadyAligned`: 现有 outline 已经和目标目录一致
- `Updated`: 成功写出新的 outline PDF

默认不会原地覆盖输入文件. 默认输出文件名规则是:

```text
outlined_<输入文件名去掉扩展名>.<原扩展名>
```

只有在 `--output` 显式传入和输入路径相同的情况下, 程序才会先写入临时文件, 再替换原文件.

## 示例

仓库里已经带了样例文件:

- `assets/sample-text.pdf`
- `assets/sample-image-and-large.pdf`
- `assets/outlined_sample-text.pdf`
- `assets/outlined_sample-image-and-large.pdf`

可以直接对照输入和输出效果.

## 常见问题

### 为什么目录页码和 PDF 页码对不上, 结果仍然可能正确?

因为程序不是直接把目录里的数字当作物理页码写入. 它会额外读取正文样本页上的可见页码, 推断 "印刷页码 -> PDF 物理页码" 的偏移关系.

### 为什么已有书签时有时不会重写?

因为程序会先读取已有 outline, 再做标题和层级归一化对比. 如果语义上已经一致, 就直接返回 `AlreadyAligned`.

### 哪些地方最容易失败?

- `pdftoppm` 不可用
- `pdftotext` 不可用, 且 OCR 也无法提供足够稳定的文字
- `tesseract` 不可用, 而 PDF 内嵌文字又不足
- 没有可用的 API key
- 目录页版式过于复杂, 导致目录识别失败
- 页面上根本没有可见印刷页码, 导致页码映射只能退化估计

## 代码入口

- 主入口: [`src/main.rs`](/Users/azazo1/pjs/rust/outliner/src/main.rs)
- 参数和配置解析: [`src/config.rs`](/Users/azazo1/pjs/rust/outliner/src/config.rs)
- LLM 调用: [`src/llm.rs`](/Users/azazo1/pjs/rust/outliner/src/llm.rs)
- PDF 渲染与页码映射: [`src/pdf_support.rs`](/Users/azazo1/pjs/rust/outliner/src/pdf_support.rs)
- Outline 读写: [`src/qpdf_outline.rs`](/Users/azazo1/pjs/rust/outliner/src/qpdf_outline.rs)
- 数据模型和归一化: [`src/model.rs`](/Users/azazo1/pjs/rust/outliner/src/model.rs)
- 进度显示: [`src/progress.rs`](/Users/azazo1/pjs/rust/outliner/src/progress.rs)

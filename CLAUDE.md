# CLAUDE.md

本文件给 Claude Code 在本仓库工作时使用。产品需求见 `docs/PRD.md`。
GPUI Kit 的详细开发规则见 `docs/gpui-kit.md`(改动 `crates/es-deck/` 下的代码前先读)。

## 项目简介
elasticsearch-deck:基于 GPUI Kit 的 Elasticsearch 桌面客户端。核心是 REST API 与 Query DSL 的语法补全,同时提供请求执行与结果浏览。

- 语言:Rust
- UI:GPUI Kit(`gpui-component` / `gpui-base`)。版本 0.9.x,尚未 1.0,API 可能变动
- 补全引擎:独立 crate,同时以 LSP 服务形式提供
- 命名:仓库与产品名 `elasticsearch-deck`;应用 crate 为 `es-deck`,构建相关 crate 使用前缀 `es-deck-`;引擎相关 crate 保持 `es-dsl-*`、`es-client`
- 商标:名称含 Elasticsearch 字样,正式发布前需核对 Elastic 的商标使用政策(未核实具体条款)

## 事实核查原则(重要)
- GPUI Kit 的 API 以**实际依赖的版本源码和文档**为准。不要凭记忆写 GPUI / GPUI Kit 的 API;不确定时先读 `~/.cargo/registry` 里的源码或运行 `cargo doc`。
- Elasticsearch 的 schema 以 `elasticsearch-specification` 仓库为准,不要手写 DSL 关键字表。
- PRD 中标注【待验证】的性能和兼容性指标,没有实测前不要在代码注释或文档里当作事实陈述。

## 仓库结构(Cargo workspace)
```
crates/
  es-dsl-core/      # 无 UI 依赖:请求行解析、容错 JSON 解析、schema 索引、补全/诊断/悬停
  es-dsl-lsp/       # LSP 适配层(独立可执行)
  es-client/        # HTTP 客户端、认证、TLS、超时、取消
  es-deck-schema/   # 构建期:把 elasticsearch-specification 转为内部索引,按 ES 版本打包
  es-deck/      # GPUI Kit 界面与状态管理
tests/              # 跨 crate 集成测试、补全基准用例
docs/               # PRD 与设计文档
```

## 架构约束
1. **依赖方向**:`es-deck → es-dsl-lsp / es-client → es-dsl-core`。`es-dsl-core` 和 `es-client` 不得依赖任何 UI crate。
2. **框架隔离**:所有 GPUI Kit 类型只出现在 `es-deck` 内;其中再建一层 `ui_adapter` 模块封装常用组件,方便版本升级时集中修改。
3. **补全逻辑只写一份**:UI 内的编辑器和外部编辑器共用同一个 LSP 实现。不要在 `es-deck` 里另写补全逻辑。
4. **不阻塞 UI 线程**:网络请求、schema 加载、mapping 解析、大 JSON 处理一律异步或放后台线程。
5. **凭据**:只存系统钥匙串。禁止写入日志、历史、配置文件、panic 信息。
6. **GPUI 相关代码**必须遵守 `docs/gpui-kit.md`;`es-deck` 内按能力分模块,不建全局 `models/`、`views/` 目录。

## 常用命令
```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo run -p es-deck            # 启动应用
cargo run -p es-dsl-lsp -- <index.json>   # 单独启动 LSP(stdio),索引由 es-deck-schema 生成
cargo run -p es-deck-schema -- <schema.json> <index.json>   # 从 elasticsearch-specification 的 schema.json 生成索引
```
提交前必须通过:`fmt`、`clippy`、`test`。

## 补全引擎开发规则
- 处理流程:切分请求块 → 匹配 endpoint → 容错解析 JSON 并计算光标处的 JSON 路径 → 沿路径在类型表中下钻 → 合并 mapping 字段 → 排序。
- 必须容错:对**任意 JSON 前缀**,引擎不得 panic,也不得返回空结果而不说明原因。
- 需要特殊处理的结构:联合类型、查询类型变体、用户自定义键(`match.<field>`、`aggs.<name>`)、简写形式、NDJSON(`_bulk`、`_msearch`)。
- 候选必须按当前集群版本过滤。
- 性能预算:补全 P95 ≤ 50 ms(本地 schema)。新增逻辑若影响热路径,附基准测试结果。
- 每新增一类补全能力,同时增加表驱动测试:`输入文本(含光标标记 |) → 期望候选`。

## 测试要求
- **单元测试**:`es-dsl-core` 覆盖率优先,表驱动。
- **属性测试**:对 JSON 前缀做 fuzz / proptest,检查无 panic。
- **UI 测试**:使用 GPUI Kit 的无头窗口集成测试,模拟键盘输入并断言补全弹层、焦点和无障碍状态。具体 API 以当前版本文档为准。
- **基准用例**:放在 `tests/completion_cases/`,来源于官方文档和 Kibana 示例,新增或修改补全行为必须同步更新。
- **集群集成测试**:使用 Docker 启动 Elasticsearch(7.x 和 8.x 各一个),默认不在普通 `cargo test` 里运行,用 feature `it-cluster` 启用。

## 安全规则(产品行为)
- 危险请求(`DELETE`、`_delete_by_query`、`_update_by_query`、含 delete 的 `_bulk`、`_close`、`_forcemerge` 等)执行前必须二次确认。
- 只读模式下必须拦截写操作。只读判断按 endpoint 语义,而不是只看 HTTP 方法(`POST _search` 是只读的)。
- 不引入遥测。若要加,必须默认关闭并先与产品确认。

## 代码风格
- 遵循 `rustfmt` 默认配置和 `clippy`(警告视为错误)。
- 库 crate 用 `thiserror` 定义错误,应用层用 `anyhow`。不在库代码里 `unwrap()` / `expect()`,测试除外。
- 公共 API 写文档注释;复杂算法写"为什么"而不是"做什么"。
- **代码注释和文档注释一律用英文,禁止中文注释**(含 `//`、`///`、`//!`、`shortcut:` 标记)。测试数据里的中文输入样例(如 IME 用例)不受此限。
- 日志用 `tracing`;不得记录请求体中可能含的敏感字段(认证头、API Key)。
- 依赖要保守:新增 crate 前检查许可证(MIT/Apache-2.0 优先)、维护状态和体积;在 PR 描述里说明理由。
- GPUI Kit 版本和 git rev 必须**固定**写在根 `Cargo.toml` 的 `[workspace.dependencies]` 中,升级单独提交。

## 工作方式
1. **调研优先,不要手搓**(强制)。写任何新模块前先调研,按此顺序找现成方案,选到哪一级就停:本仓库已有代码 → GPUI Kit 自带组件 → 官方/标准方案(`elasticsearch-specification`、LSP 官方类型库、Kibana Console 的补全实现)→ 成熟第三方 crate → 最后才手写。调研结论(候选、取舍、许可证、维护状态)记入 `docs/m0-findings.md` 或设计文档,再动手;手写的部分要说明为什么前几级都不适用。
2. 动手前先读相关 crate 的现有代码和测试,沿用已有模式。
3. 改动尽量小、可回滚;一个提交只做一件事。
4. 凡是涉及 GPUI Kit 用法的改动,先确认 API 存在(读源码或文档),再写代码。
5. 修改 UI 后,若能无头运行,先渲染和断言再交付;不能验证的部分在说明里写明"未验证"。
6. 不确定的产品决策(范围、行为、文案)先提问,不要自行扩展 PRD 范围。
7. 完成后汇报:改了什么、跑了哪些命令、结果如何、有哪些未验证项。

## 当前阶段与优先级
当前处于 **M0 原型验证**。优先验证并记录结论到 `docs/m0-findings.md`:
1. 中文输入法(预编辑、候选窗定位)在编辑器中是否可用。
2. 最小 LSP:仅补全 `_search` 请求里 `query` 下的关键字,打通"编辑器 → LSP → 弹层"。
3. Windows / macOS / Linux 三平台能否构建并运行。
4. 10 万行表格滚动性能。

若第 1 项或框架稳定性验证失败,按 PRD 3.3 节切换备选方案(Tauri + Monaco),并保留 `es-dsl-core`、`es-dsl-lsp` 复用。

## 不要做
- 不要手写 DSL 关键字表来代替 schema。
- 不要跳过调研直接手写通用能力(JSON 解析、LSP 协议、HTTP、schema 转换等)。
- 不要在 `es-dsl-core` 中引入 UI 或网络依赖。
- 不要把集群地址、凭据或真实数据提交进仓库。
- 不要跳过测试或用 `#[ignore]` 掩盖失败。
- 不要修改 PRD 范围之外的功能(如可视化仪表盘、集群运维管理)。

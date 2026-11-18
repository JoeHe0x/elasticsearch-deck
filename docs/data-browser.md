# Data Browser 设计

状态:**首版已实现,尚不是产品水准**(见 §9 的已做/未做清单)。§8 未回答的问题按推荐项处理;UI 未经真实窗口渲染验证,ES 行为未对照真实集群。需求来源:`docs/PRD.md` 5.9(2026-11-16 新增范围,经产品确认)。
本文只在 PRD 范围内展开;凡 PRD 未定的行为列在"待确认",不自行决定。

## 1. 目标与非目标

目标:从左侧树点开一个索引或别名,得到一个**只有表格**的 Tab,可翻页、按列排序、用过滤框筛选。

非目标(PRD 明确不做):新增行、删除行、编辑单元格;自研过滤语法;请求编辑器。

## 2. 现状(读代码得出,未运行)

- 左侧树双击索引/别名走 `Console::open_named`(`console.rs`),生成一段 `_search` 文本,打开一个**REST Console** Tab。这不是 Data Browser。
- Tab 是 `Console.tabs: Vec<String>`(名字),文本按连接存在 `Library` 里,Tab 之间靠名字索引。目前没有"Tab 类型"。
- 表格是 `HitsTable`(`hits_table.rs`):一次性渲染 `es_client::Hits`,无排序、无分页。
- `es_client::parse_hits` 已能把响应转成列和行,并给出 `total`(含 `10000+` 形式)。
- `Request` / `Client::send` 已可发 `POST {index}/_search`;危险与只读判断在 `es-dsl-core/src/safety.rs`。
- 内核表格组件(gpui-component 0.10.0 `table`)提供 `TableDelegate::perform_sort`、`load_more`、`ColumnSort`;其中 `perform_sort` 默认是空实现,需要自己实现并发请求。

## 3. 调研结论(CLAUDE.md "调研优先")

| 能力 | 选择 | 理由 |
|---|---|---|
| 表格渲染、虚拟滚动、列宽、排序指示 | 复用 GPUI Kit `table` + 现有 `HitsTable` | 本仓库已有;不另写 |
| 响应转行列 | 复用 `parse_hits` | 已有且有测试 |
| 过滤语法 | KQL 子集,客户端翻译成 Query DSL;翻译不了则回退原样 `query_string` | 产品确认要 KQL(见 §4.5);没有可用的 Rust KQL→DSL crate(搜到的都是 Kusto),所以手写小解析器,放在 `es-client/src/browse.rs` |
| 翻页 | `from`/`size` | PRD 规定;`search_after` 不能随机跳页,不符合"分页" |
| 构造请求体 | `serde_json::json!` 拼装 | 已是依赖,不新增 crate |

不新增依赖。

### 3.1 首要参考:Elasticsearch IDEA

按产品要求,以它为首要参考。来源是其官网首页和文档页(`/docs/features/searching-data/`、`/response-view/`、`/elasticsearch-tool-window/`),2026-11-18 读取;没有安装试用,凡文档没写的都标"未说明",不推测。

它的做法(文档所述):
- 入口:在工具窗口里双击索引或别名,打开搜索结果面板;也可以手写请求。
- 视图:`View as` 按钮在 Table / JSON Hits(只含 hits)/ JSON Response(完整响应)间切换。
- 表格:点列名排序(也可从列右键菜单选 Sort);列右键菜单有 `Hide field`;鼠标悬停列名显示该字段的 mapping。
- 分页:工具栏上"首页 / 上一页 / 下一页",页大小在中间,可从预设列表改;`Reload Page`(⌘R)刷新。
- 过滤:KQL,输入时提示字段名和运算符;只做过滤,不排序不聚合;过滤会给请求加 `filter` 子句和 `highlight`。
- 写操作:行内新增/删除/提交,以及对话框式的新增/更新/删除文档;另有导入、导出(剪贴板或文件)、`Data Extractors`(复制行的格式)。
- 它的表格始终和一条请求绑定,工具栏有"同步请求体"开关,翻页和过滤会改写请求编辑器里的请求。
- 文档**没写**:页大小的具体档位、哪些列可排序、非法 KQL 的报错表现、`max_result_window` 处理、列宽与列序调整、嵌套对象显示、行详情。

与本方案的取舍:

| 它的能力 | 本方案 | 原因 |
|---|---|---|
| 双击索引/别名打开 | 采用 | 与现有交互一致 |
| 点列名服务端排序 | 采用 | PRD 要求,见 §4.4 |
| 悬停列名显示 mapping 类型 | 建议采用(§8-8) | 成本低;`ClusterMeta` 已缓存 mapping;PRD 未写 |
| 隐藏列 | 建议采用(§8-9) | 稀疏索引最多 60 列(`hits.rs`),需要收窄;PRD 未写 |
| 首页/上一页/下一页 + 页大小 + 刷新 | 采用;刷新是新增建议(§8-10) | PRD 要求分页;刷新 PRD 未写 |
| Table / JSON 视图切换 | 不做 | PRD 写"只有表格";需要看原文时用 REST Console |
| KQL + 输入提示 | **采用**(产品确认) | KQL 子集手写翻译,字段名补全带 mapping 类型;未覆盖的写法回退 Lucene,见 §4.5 |
| 与请求编辑器同步 | 不做 | Data Browser 没有编辑器(PRD) |
| 新增/更新/删除、导入 | 不做 | PRD 5.9 只读 |
| 导出 | 不在本期 | 属 V6(P1) |

### 3.2 其他同类产品对照

来源只有各产品的官方文档/README(2026-11-18 读取),没有安装试用,界面细节未实测。

| 能力 | Kibana Discover | Dejavu | Elasticvue | 本方案 |
|---|---|---|---|---|
| 分页 | 每页行数菜单(默认 100),末尾"加载更多";初始样本量可设,上限为 `discover:sampleSize` | 分页,页大小可调,另提到无限滚动 | 文档未说明 | `from`/`size` 分页(PRD 规定) |
| 排序 | 点列头单字段排序;"Sort fields"可多字段并拖动调序;默认按时间字段倒序 | 列排序为浏览器本地操作 | 文档未说明 | 服务端单字段排序;不做本地排序,因为只会排当前页,容易误导 |
| 过滤 | 查询栏(KQL 带字段/运算符提示)+ 过滤条 | 字段级 facet 过滤、全局搜索、Query DSL | 文档只列"Rest queries" | 单行输入框,KQL 子集 + Lucene 回退,字段名补全 |
| 列 | 拖动/左右移动调序、拖边缘调宽、字段列表增删列 | 可只显示部分列 | 文档未说明 | 沿用 `parse_hits` 的列并集,列宽可调(已有) |
| 行详情 | 文档未覆盖 | 未说明 | 未说明 | 见下 |
| 导出 | 文档未覆盖 | 过滤后的视图导出 JSON/CSV | 未说明 | 属 V6(P1),不在本期 |
| 写操作 | 无 | 编辑、删除、导入 | 搜索与编辑文档 | **不做**(PRD 5.9 只读) |

对本方案的启示(均为建议,是否采纳见 §8):
1. **Kibana 用"加载更多"而不是页码**,以避开深翻页。我们的 PRD 写的是分页,但 `max_result_window` 的限制同样存在,所以窗口上限提示不能省。
2. **服务端排序**是正确做法。Dejavu 的本地排序只对已加载的行有效,不采用。
3. **多字段排序**(Kibana)PRD 没要求,本期只做单字段。
4. **点击单元格/行看完整文档**是这类产品的常见需求(表格单元格有 400 字截断,见 `hits.rs`),但上述文档都没写清它,PRD 也没要求。列为待确认。
5. Dejavu 的 facet 过滤、Kibana 的字段列表增删列都超出 PRD 的"过滤框"范围,**不做**。

## 4. 行为

### 4.1 打开
- 入口:树里的索引/别名。单击仅选中,双击打开(沿用现有交互);现有"双击打开 REST Console"改为打开 Data Browser(待确认,见 §8-1)。
- 同一连接、同一目标已有 Data Browser Tab 时切到它,不重复开。
- 标题 `目标名 [连接名]`(PRD 5.9)。

### 4.2 请求
每次翻页、排序、过滤变化都发一次:

```
POST /{target}/_search
{
  "from": <page * size>,
  "size": <size>,
  "track_total_hits": true,
  "query": <kql(filter)>,   // 翻译失败时为 { "query_string": { "query": "<filter>" } };过滤框为空时省略
  "sort": [ { "<field>": "asc|desc" } ]                  // 未排序时省略
}
```

- `track_total_hits: true` 用于显示总数,否则超过 10000 只返回下界。代价【待验证】:大索引上更慢,是否改成"仅在第一页取精确总数"见 §8-3。
- 请求走现有 `Client::send`,在后台任务里执行;新请求发出时丢弃(abort)上一个未完成的请求,避免旧结果覆盖新结果。
- 过滤文本先经 `kql()` 翻译(§4.5),翻译不了才原样作为 `query_string.query`。语法错误时 ES 返回 400,**显示服务端返回的原因**(从 `error.root_cause[0].reason` 取,取不到则显示原始 body 摘要),不吞错。

### 4.3 分页
- 页大小默认 50,可选 25/50/100/200(具体档位待确认,§8-2)。
- 底部栏:`‹ 上一页 | 第 N 页 / 共 M 页 | 下一页 ›`、总数、页大小。
- **窗口上限**:`from + size > index.max_result_window`(默认 10000)时 ES 会拒绝。处理:
  - 翻到会超出的页之前,禁用"下一页"并显示原因,**不静默截断**;
  - 窗口值读取自 `GET /{target}/_settings?include_defaults=true&filter_path=*.settings.index.max_result_window,*.defaults.index.max_result_window`,读不到时按 10000 处理;别名对应多个索引时取最小值。
  - 提示文案要说明:需缩小过滤条件或排序后再查。PRD 标注【待验证:翻页超出窗口的处理方式】,本方案是候选,须经确认。

### 4.4 排序
- 点击表头循环:无 → 降序 → 升序 → 无(与内核 `perform_sort` 的循环一致)。
- 只有"可排序"的列才显示排序入口。可排序性来自目标索引的 mapping(`es-client` 的 `ClusterMeta` 已缓存 mapping 字段及类型):
  - `keyword`、数值、日期、布尔等有 doc values 的类型可排;
  - `text` 字段本身不可排,若有 `.keyword` 子字段则改为按子字段排,否则该列不可排;
  - 其余类型不可排(保守处理,避免产生 ES 400)。
- 列是 `_source` 顶层键的并集(现有 `parse_hits` 行为),嵌套对象/数组列按顶层键显示,且不可排。
- 每次改变排序回到第一页。

### 4.5 过滤框
- 单行编辑器(`EditorState`,因为只有它支持补全钩子),回车应用,应用时回到第一页。留空 = 全部文档。
- **KQL 子集**(`es_client::browse::kql`,纯函数,表驱动测试):
  - `or` 优先级低于 `and`,`not` 为前缀,支持括号,关键字不分大小写;
  - `field : value` → `match`;`field : "phrase"` → `match_phrase`;`field : wild*` → `query_string`;`field : *` → `exists`;
  - `> >= < <=` → `range`;裸值 → `query_string`。
- **回退**:`a:`、结尾的 `and`、未闭合括号、`a:[1 TO 2]`、未加引号的多词值、单独的 `:` 都返回 `None`,此时整段文本作为 `query_string` 发出,所以旧的 Lucene 写法仍可用。
- **补全**(`es-deck/src/completion.rs` 的 `FieldCompletion`):只补字段名(非 nested),label 是字段名、detail 是 mapping 类型;光标在值或未闭合引号里时不弹。没有 `and/or/not` 关键字补全,没有语法着色。
- 已知限制:字段名含空格不支持;值一律按字符串发送,不感知字段类型。

### 4.6 状态
空闲 / 加载中(沿用 Send 按钮的 loading 风格)/ 成功 / 错误(服务端原因)/ 无数据(显示"没有匹配的文档")。切换连接或目标不存在(404)时显示错误,不崩溃。

### 4.7 只读与安全
- Data Browser 只发 `_search`,属只读;连接的只读模式不拦截它,也不改变现有写操作确认规则(PRD 5.9)。
- 不执行用户任意请求,因此不经过危险请求确认流程;请求在代码里构造,必须有测试证明只会生成 `POST /{target}/_search`。
- 目标名需要 URL 编码后放入路径,防止名字里的特殊字符改变请求路径(星号、逗号等通配写法属于 ES 语义,不在此限制;是否允许见 §8-4)。

## 5. 架构落点

约束:GPUI 类型只出现在 `es-deck`;网络不阻塞 UI;不建全局 `models/`、`views/`。

- **`es-client`**(无 UI):新增一个纯函数模块,例如 `browse.rs`:
  - `SearchPage { from, size, filter, sort }` → `Request`(拼请求体);
  - `parse_hits` 之外只加"取错误原因"和"窗口上限"的解析。
  - 全部可表驱动单测,不触网。
- **`es-deck`**:
  - 新模块 `data_browser.rs`:持有一个 `Entity<TableState<HitsTable>>`、过滤框 `InputState`、页码/大小/排序状态、当前 `Task`。
  - `HitsTable` 增加:列的排序状态、`perform_sort` 回调通知 Data Browser。保持现有 REST Console 的用法不变。
  - `Console` 的 Tab 需要有"类型"。最小做法:`tabs` 的元素从 `String` 变成带 `kind` 的小枚举,Data Browser Tab 不读写 `Library.console` 文本,而是存自己的小状态(目标、过滤、排序、页大小)。
- 所有 GPUI Kit 组件调用经 `ui_adapter` 层(若当前代码尚未建立,沿用现有 `Console` 的做法,并在汇报里写明)。

## 6. 持久化

PRD:重启后恢复 Tab 与请求文本,**不保存凭据与响应体**。Data Browser 恢复:目标、过滤文本、排序、页大小,**不恢复页码和结果**(重启后重新查询第一页)。存放位置与现有 console 存储同目录,格式待与 `saved.rs` 现有格式对齐后确定。

## 7. 测试

- `es-client`:表驱动,输入 `(from, size, filter, sort)`,断言请求体 JSON;空过滤不含 `query`;未排序不含 `sort`;错误响应解析出 `reason`;窗口上限解析(单索引、别名多索引取最小、缺失回退 10000)。
- 属性测试:任意过滤字符串都只产生合法 JSON 且路径固定为 `/{target}/_search`(过滤文本只进请求体)。
- 排序可排性:mapping 类型 → 是否可排、改用哪个子字段,表驱动。
- UI:无头窗口测试——打开后列头出现;点击表头触发排序请求;翻到窗口上限时"下一页"禁用并出现提示;快速连续操作只采用最后一次结果。
- 集群集成测试(feature `it-cluster`):对 7.x、8.x 各跑一遍分页与排序,并验证 `max_result_window` 提示。默认不跑。

## 8. 待确认(需要产品决定,未自行扩展)

1. 双击索引现在打开的是 REST Console。Data Browser 上线后:双击改为开 Data Browser,REST Console 走右键菜单?还是并存两个入口?(右键菜单已做:"Open in Data Browser" Ctrl+↓、"Open in REST Console" Ctrl+Enter;双击的取舍仍未经产品确认。)
2. 页大小档位与默认值。
3. 总数:每页都 `track_total_hits: true`,还是只在第一页和过滤变化时取一次精确总数,翻页时沿用?
4. 目标是否允许通配或多索引(`logs-*`)?树里点开的只有具体索引和别名,所以默认只支持这两种。
5. 窗口超限的处理方式(§4.3 为候选:禁用下一页并说明原因)。
6. 是否需要"点行查看完整文档"(同类产品的常见做法,PRD 未提)。**已按默认做法实现**:点行在右侧展开完整 JSON,带复制;交互未经产品确认。
7. 是否改用 Kibana 式"加载更多"替代页码(PRD 写的是分页,改动属范围变化)。
8. 悬停列名显示 mapping 类型(Elasticsearch IDEA 有;PRD 未写)。**已实现**。
9. 列右键"隐藏列"(Elasticsearch IDEA 有;PRD 未写)。
10. 工具栏"刷新"按钮,重发当前页请求(Elasticsearch IDEA 有;PRD 未写)。
11. 过滤框是否接受 KQL:**已确认要 KQL**(2026-11-18),按 §4.5 实现;PRD 5.9 的"原样 `query_string`"需随之更新。

## 9. 实现状态与未验证

### 已实现(有单元测试或 httpmock 测试)
- 请求构造(`from/size/track_total_hits/query/sort`)、KQL 翻译与 Lucene 回退、服务端错误原因提取、`max_result_window` 读取并停在窗口上限、按 mapping 决定可排序列(text → `.keyword`)。
- 表头排序、翻页、页大小切换、刷新按钮、字段名补全、"没有匹配的文档"空态;新请求替换旧请求(丢弃 `Task` 即取消);
- 单元格按类型显示(null、数字右对齐、数组/对象摘要 `a, b, c +N`)、列宽按内容估算、省略号加悬停全文、表头悬停显示 mapping 类型、列可拖动调序;斑马纹保留(`stripe(true)`)。
- 点行展开右侧完整 JSON 面板(复制、关闭),行右键"Copy row as JSON";状态栏同时显示总耗时和集群 `took`;索引/别名右键菜单与快捷键。Console 结果表与 Data Browser 共用 `hits_table` 的表格、列菜单与行详情;两者的排序不同——Data Browser 把排序写进自己生成的 `_search`,Console 只对已经返回的行做客户端排序,不改写用户的请求。

### 未做(相对产品水准的差距)
- 复制单元格值、导出。
- 列隐藏已做:工具栏"列"菜单勾选,隐藏状态在标签页内跨翻页保留(未渲染验证)。
- `search_after` 深翻页;窗口超限只做到"停住并说明"。
- 过滤框:语法着色、关键字补全、含空格字段名、过滤历史、类型感知的值。
- 持久化(§6)未实现;双击入口与 REST Console 的取舍(§8-1)按推荐项处理,未经产品确认。

### 未验证
- **UI 从未在真实窗口里渲染过**:补全弹层、Enter 与补全菜单的冲突、32px 编辑器高度、斑马纹与类型化单元格外观、详情面板、列拖动、右键菜单快捷键提示都只是编译和无头测试通过。REST Console 结果表的列菜单、详情面板与客户端排序同样只有无头断言。
- **没有对照真实集群**:mock 之外,`match`/`range` 作用于 date/ip 字段、7.x 与 8.x 的 `track_total_hits` 和 settings 路径、错误体结构,都没实测;没有 `it-cluster` 集成测试。
- 加载/取消的实际观感、大索引上 `track_total_hits: true` 的耗时【待验证】。

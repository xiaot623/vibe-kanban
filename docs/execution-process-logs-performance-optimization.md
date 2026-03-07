# Execution Process Logs 性能瓶颈与优化思路

## 背景

当前 `execution_process_logs` 在大型会话下会出现明显卡顿：前端加载会话历史通常需要几十秒。  
以下结论基于现有代码路径和 `pi_agent_rust` 的会话存储优化设计对比。

## 当前瓶颈（按影响排序）

1. 全量读取 + 全量反序列化
   - `crates/db/src/models/execution_process_logs.rs` 的 `find_by_execution_id` 直接 `fetch_all` 全量日志。
   - `parse_logs` 会对所有记录逐行 `serde_json::from_str`，大型会话 CPU/内存压力非常大。

2. 历史 normalized 日志需要“重建 + 再归一化”
   - `crates/services/src/services/container.rs` 的 `stream_normalized_logs` 在无内存 store 时：
     - 先全量拉取 DB logs；
     - 再构建临时 `MsgStore`；
     - 再跑 `normalize_logs`。
   - 该路径在历史回放阶段开销大，且会重复计算。

3. 写入路径过于细粒度（每条日志一次 INSERT）
   - `spawn_stream_raw_logs_to_db` 对每条 `Stdout/Stderr` 立即执行一次 `append_log_line`。
   - 高频小事务对 SQLite 写放大明显。

4. 前端历史回放成本高
   - `streamJsonPatchEntries` 每个 patch 都 `structuredClone(snapshot)` 再 `applyPatch`。
   - 历史回放 patch 数量大时，形成高频大对象拷贝。

5. 前端聚合阶段反复全量排序/flatten
   - `useConversationHistory` 在大量历史 execution process 下会重复排序和拼接 entries，进一步放大耗时。

## 可借鉴的 `pi_agent_rust` 设计

1. 分段日志 + 偏移索引（offset index）
   - 通过 sidecar index 支持按偏移读取、按范围读取、tail 读取，避免全量扫描。

2. 懒加载策略
   - 根据 session 大小选择 `Full / ActivePath / Tail` 加载模式，降低首次打开成本。

3. 增量写入 + 周期性 checkpoint
   - 常态增量 append，周期性做整理，避免每次全量重写。

4. 指标先行
   - 对 save/load/append/index 各阶段打点，确保优化可量化、可回归。

## 优化方案（建议分阶段落地）

### Phase 0（低风险，最快见效）

1. 增加分页/游标读取
   - 为日志增加单调递增 `seq`（或使用可稳定排序的主键）。
   - 新增接口：`after_seq + limit`，历史默认先返回最近 N 条（如 200~500），前端按需继续拉取。

2. 增加日志类型列，减少无效反序列化
   - 写入时记录 `msg_type`（`stdout/stderr/json_patch/...`）。
   - 查询时直接按类型过滤，减少 DB->应用层无用数据传输与 JSON 解析。

3. 前端回放节流
   - 对 patch 应用和渲染做节流（`requestAnimationFrame` 或固定 30~50ms 批处理）。
   - 降低“每条消息一次状态更新”的渲染抖动。

### Phase 1（中等改动，收益大）

1. 持久化 normalized 结果（避免重复 normalize）
   - 历史打开时直接回放已归一化日志，不再临时重建 store 并重新归一化。
   - 可通过“raw + normalized 双通道存储”或“按 execution 缓存快照”实现。

2. 批量写入日志
   - 将单条 INSERT 改为批量 flush（例如 50~200 条或 100ms 窗口），减少事务开销。
   - 配合 WAL 和合理 checkpoint 策略降低写放大。

### Phase 2（结构升级）

1. 引入分段存储 + 偏移索引
   - 参考 `pi_agent_rust`：日志按段写入，单独维护 offset index。
   - 支持 `tail`、范围、按 entry 快速定位，避免全量扫描与大对象拼装。

2. 引入快照/检查点
   - 每 X 条生成一次会话快照，历史加载采用“快照 + 增量 tail”。
   - 将首屏时间从 O(全量历史) 降到 O(快照+最近增量)。

## 建议优先级

1. 先做分页读取 + 前端按需加载（Phase 0）
2. 再做 normalized 持久化（Phase 1）
3. 最后推进分段索引化存储（Phase 2）

## 预期收益（经验值）

- 首屏历史加载时间：可从“几十秒”降到“秒级”甚至“亚秒级首屏可见”
- 后端 CPU 峰值：显著下降（减少全量 JSON 解析和重复 normalize）
- 前端卡顿：明显改善（减少全量 clone 与高频 render）


# pending/ — 待审批记忆

候选记忆先落这里，**用户批准后**才写进 `memory/MEMORY.md`。

| 文件 | 内容 |
|---|---|
| `candidates.jsonl` | 待审批候选，每行一个 JSON |
| `rejected.jsonl` | 被拒记录（用于避免重复提同一个候选） |

## 候选字段

```json
{
  "id": "唯一 id",
  "at": "ISO8601 时间",
  "scope": "user | project",
  "kind": "fact | preference | decision | lesson",
  "content": "记忆正文",
  "source": "来源（哪天/哪个文件）",
  "fingerprint": "内容指纹，用于去重"
}
```

**为什么要有审批环节**：记忆是**滚动累积**的，一条错的条目会一直影响
后续所有对话，而且不会自己过期。所以宁可多一道人工关卡。
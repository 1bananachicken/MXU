---
name: code-review
description: >-
  MXU 全项目代码审查流水线。审查 React 前端、Rust 后端、PI V2 协议实现、状态管理、
  i18n、安全性等。Use when the user says "全项目review"、"代码审查"、"review MXU"、"code review".
---

# MXU Code Review Pipeline

全项目级代码审查 → 发现分类 → 输出报告。

## Phase 1: 探索 & 拆分 Review Unit

1. 扫描项目结构
2. 按模块拆分为 12-16 个 Review Unit
3. 优先级：P0（安全/数据完整性）、P1（可靠性/UX）、P2（代码质量）

### 推荐 Unit 划分维度

| 维度 | 关注点 |
|------|--------|
| PI V2 协议实现 | interface.json 解析正确性、任务配置映射 |
| Maa FFI | Rust 端 Maa 调用正确性、错误处理、资源释放 |
| 更新系统 | updateService 安全性、下载完整性校验、回退机制 |
| 状态管理 | Zustand store 设计、状态一致性、竞态 |
| 组件质量 | React 组件拆分、渲染性能、hooks 使用 |
| Tauri 权限 | capabilities 配置完整性、最小权限原则 |
| i18n | 多语言覆盖完整性、key 同步 |
| Agent 集成 | Agent 启动/通信逻辑、错误恢复 |
| 安全 | 文件系统访问、网络请求、IPC 安全 |
| CI/CD | 多平台构建、发布流程 |

## Phase 2: 并行 Review

每个 review subagent prompt 模板：

```
你是 Tauri 桌面应用审查员。审查以下文件，找出：
1. Bug（逻辑错误、状态不一致、竞态条件）
2. 安全问题（IPC 注入、文件路径穿越、权限过宽）
3. 性能（不必要重渲染、大对象频繁拷贝）
4. UX（错误提示缺失、操作反馈不足）

文件范围：{files}
重点关注：{focus_areas}

输出 Top 5 问题。
```

## Phase 3: 汇总 & 分类

| 分类 | 含义 |
|------|------|
| 安全 | IPC/文件/网络安全问题 |
| 协议 | PI V2 实现偏差 |
| 可靠性 | 更新系统、Agent 通信、错误恢复 |
| 性能 | 渲染/内存/网络效率 |
| 质量 | 组件设计、i18n、代码风格 |

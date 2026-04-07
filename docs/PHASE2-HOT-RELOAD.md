[3054 chars] # Phase 2 PRD: 双层架构 + Extensions 热加载

> 状态：draft → review
> 日期：2026-04-07
> 讨论：泽平 + seam_walker
> Repo：github.com/d5z/heart-portal

## 目标

Portal 双层设计：内层 kernel 稳定无以复加，外层 extensions being 可 DIY + 热加载。

## 架构

```
Portal 进程（...
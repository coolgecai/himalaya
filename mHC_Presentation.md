# mHC: Manifold-Constrained Hyper-Connections
## 演讲报告

---

## Slide 1: 标题页

### **mHC: Manifold-Constrained Hyper-Connections**

**DeepSeek-AI | arXiv:2512.24880 | 2026 年 1 月**

---

## Slide 2: 演讲大纲

1. **研究背景** - 残差连接的演进
2. **问题定义** - Hyper-Connections 的瓶颈
3. **核心方法** - mHC 架构设计
4. **理论保证** - 流形约束的数学基础
5. **工程优化** - 基础设施设计
6. **实验结果** - 性能与稳定性分析
7. **结论展望** - 未来研究方向

---

## Slide 3: 研究背景 - 残差连接范式

### 标准残差连接 (ResNet, 2016)

$$x^{l+1} = x^l + \mathcal{F}(x^l, W^l)$$

### 多层的递归展开

$$x^L = x^l + \sum_{i=l}^{L-1} \mathcal{F}(x^i, W^i)$$

**恒等映射 (Identity Mapping)** 保证了：
- 浅层信号直接传递到深层
- 梯度回传的数值稳定性

```
        ┌─────────────┐
  x^l ──►│   Layer ℱ   │──► x^{l+1}
        │             │
        └─────────────┘
              │
              ◄──────┘ (Skip Connection)
```

---

## Slide 4: 研究背景 - Hyper-Connections

### 残差流的扩展

$$x^{l+1} = H^{l}_{\text{res}} x^l + H^{l\top}_{\text{post}} \mathcal{F}(H^{l}_{\text{pre}} x^l, W^l)$$

**关键变化：**
| 组件 | 维度 | 功能 |
|------|------|------|
| $x^l$ | $1\times C \to n\times C$ | n-stream 残差流 |
| $H^{l}_{\text{res}}$ | $n\times n$ | 残差流内特征混合 |
| $H^{l}_{\text{pre}}$ | $1\times n$ | 聚合为层输入 |
| $H^{l}_{\text{post}}$ | $1\times n$ | 层输出映射回流 |

```
                    ┌─────────────────┐
                    │   H_post        │
                    │     │           │
   x^l ─►[H_pre]───►│   Layer ℱ      │──►
                    │             │──►│
                    │             ├──►│◄───
                    │  H_res ◄────┘   │   ◄───
                    └─────────────────┘
```

---

## Slide 5: 问题定义 - 数值不稳定性

### 多层 HC 的递归展开

$$x^L = \left( \prod_{i=1}^{L-l} H^{L-i}_{\text{res}} \right) x^l + \sum_{i=l}^{L-1} \left( \prod_{j=1}^{L-1-i} H^{L-j}_{\text{res}} \right) \mathcal{F}(H^{i}_{\text{pre}} x^i, W^i) H^{i\top}_{\text{post}}$$

### 核心问题

**复合映射偏离恒等性质：**
$$\prod_{i=1}^{L-l} H^{L-i}_{\text{res}} \quad \text{不保持全局均值}$$

### 后果
- **信号爆炸**（前向传播）
- **梯度消失/爆炸**（反向传播）
- **训练不稳定**

```
Amax Gain Magnitude
   │
3000┼         ╱◄───── HC 峰值
 100┼       ╱
  10┼     ╱
   1┼───╱─── mHC 稳定在~1
   0┼─────┼─────┼────
     0    30    60  Layers
```

---

## Slide 6: 问题定义 - 系统开销

### 每 Token 内存访问成本对比

| 方法 | 读取 (Elements) | 写入 (Elements) |
|------|-----------------|-----------------|
| **Residual Connection** | $2C$ | $C$ |
| **Hyper-Connections** | $(5n+1)C + n^2 + 2n$ | $(3n+1)C + n^2 + 2n$ |

### Memory Wall 问题
- HC 的 I/O 成本 ≈ $n$ 倍增加
- 额外激活值需要梯度检查点
- 流水线并行通信成本 $n$ 倍增加

---

## Slide 7: 核心方法 - mHC 架构

### Manifold-Constrained Hyper-Connections

$$\mathcal{M}^{\text{res}} \mathrel{\vcenter{\baselineskip10pt\hruleheight1pt\hrule\atop\hbox{\small def}}} \left\{ H^l_{\text{res}} \in \mathbb{R}^{n\times n} \mid H^l_{\text{res}} \mathbf{1}_n = \mathbf{1}_n, \; \mathbf{1}_n^\top H^l_{\text{res}} = \mathbf{1}_n^\top, \; H^l_{\text{res}} \geq 0 \right\}$$

### 双随机矩阵约束

$$H^l_{\text{res}} \in \text{Birkhoff Polytope}$$

```
        ┌─────────────────────────┐
        │    PM(Birkhoff)         │
        │    双随机投影           │
 x^l ──►│  H_res (行列和=1)       │──► 特征凸组合
        │                         │
        │  ◄─── 凸包 ───►         │
        │  排列矩阵集合           │
        └─────────────────────────┘
```

---

## Slide 8: 核心方法 - 参数化与投影

### 动态与静态映射计算

**第一步：获取未约束映射**
$$
\begin{cases}
\tilde{x}^l = \text{RMSNorm}(\text{vec}(x^l)) \\
\tilde{H}^l_{\text{pre}} = \alpha^l_{\text{pre}} \cdot (\tilde{x}^l \phi^l_{\text{pre}}) + b^l_{\text{pre}} \\
\tilde{H}^l_{\text{post}} = \alpha^l_{\text{post}} \cdot (\tilde{x}^l \phi^l_{\text{post}}) + b^l_{\text{post}} \\
\tilde{H}^l_{\text{res}} = \alpha^l_{\text{res}} \cdot \text{mat}(\tilde{x}^l \phi^l_{\text{res}}) + b^l_{\text{res}}
\end{cases}
$$

**第二步：流形投影**
$$
\begin{cases}
H^l_{\text{pre}} = \sigma(\tilde{H}^l_{\text{pre}}) \\
H^l_{\text{post}} = 2\sigma(\tilde{H}^l_{\text{post}}) \\
H^l_{\text{res}} = \text{Sinkhorn-Knopp}(\tilde{H}^l_{\text{res}})
\end{cases}
$$

---

## Slide 9: 核心方法 - Sinkhorn-Knopp 算法

### 迭代归一化过程

$$M^{(t)} = T_r T_c (M^{(t-1)})$$

其中：
- $M^{(0)} = \exp(\tilde{H}^l_{\text{res}})$ （指数确保正值）
- $T_r$ = 行归一化，$T_c$ = 列归一化
- $t_{\max} = 20$ 次迭代（收敛到双随机矩阵）

### 算法流程

```
  M = exp(H_tilde)
      │
      ▼
  ┌────────┐     ┌────────┐     ┌────────┐
  │ 行归一化 │───►│ 列归一化 │───►│ 行归一化 │───► ...
  └────┬───┘     └────────┘     └────┬───┘
       │                              │
       └─────────── 20 iterations ────┘
                    │
                    ▼
           M^{(20)} ≈ Doubly Stochastic
```

---

## Slide 10: 理论保证

### 三大理论性质

| 性质 | 数学表述 | 意义 |
|------|----------|------|
| **1. 范数保持** | $\|H^l_{\text{res}}\|_2 \leq 1$ | 非扩张映射，防止梯度爆炸 |
| **2. 复合闭包性** | $A,B \in \mathcal{M}^{\text{res}} \implies AB \in \mathcal{M}^{\text{res}}$ | 多层组合仍为双随机矩阵 |
| **3. 几何解释** | $\mathcal{M}^{\text{res}} = \text{conv}(\text{Permutation Matrices})$ | 特征融合为排列的凸组合 |

### 信息混合单调性

双随机矩阵的重复应用单调增加流间信息混合

---

## Slide 11: 工程优化 - Kernel Fusion

### 优化的计算图

$$
\begin{align*}
\varphi^l &: \text{[FC 融合，吸收 RMSNorm 权重]} \\
x^l_{\text{vec}} &: \text{[1, nC], bfloat16} \\
\tilde{z} &= x^l_{\text{vec}} \varphi^l \\
r &= \sqrt{x^l_{\text{vec}} \cdot x^l_{\text{vec}} / nC} \\
\tilde{H}_{\text{pre/post/res}} &= \frac{1}{r} [\alpha \cdot \tilde{z} + b] \\
H_{\text{pre}} &= \sigma(\tilde{H}_{\text{pre}}) \\
H_{\text{post}} &= 2\sigma(\tilde{H}_{\text{post}}) \\
H_{\text{res}} &= \text{Sinkhorn-Knopp}(\tilde{H}_{\text{res}})
\end{align*}
$$

### Kernel 设计要点
- 三个专用 Kernel 分别计算 $H_{\text{pre}}, H_{\text{post}}, H_{\text{res}}$
- 使用 TileLang 框架实现混合精度优化
- 融合两次扫描，最大化内存带宽利用率

---

## Slide 12: 工程优化 - 选择性重计算

### 重计算块大小优化

$$L_r^* = \arg\min_{L_r} \left( nC \times \left\lceil \frac{L}{L_r} \right\rceil + (n+2)C \times L_r \right) \approx \sqrt{\frac{nL}{n+2}}$$

### 存储策略

| 激活类型 | 大小 | 存储策略 |
|----------|------|----------|
| $x^{l_0}$ | $nC$ | 每 $L_r$ 层存储一次 |
| $\mathcal{F}(H^l_{\text{pre}}x^l, W^l)$ | $C$ | 每层存储 |
| $H^l_{\text{pre}} x^l$ | $nC$ | 重计算 |
| RMSNorm 项 | $C$ | 重计算 |

---

## Slide 13: 工程优化 - DualPipe 通信重叠

### 扩展的流水线调度

```
┌─────────────────────────────────────────────────────────┐
│  Normal Compute Stream                                  │
│  ┌────────┐  ┌────────┐  ┌────────┐  ┌────────┐        │
│  │ ATTN F │  │ MLP F  │  │ ATTN B │  │ MLP B  │        │
│  └────────┘  └────────┘  └────────┘  └────────┘        │
│                            │                            │
│                            ▼                            │
│  ┌─────────────────────────────────────────────────┐    │
│  │          Communication Stream                    │    │
│  │  PP Send/Recv (F)  │  PP Send/Recv (B)           │    │
│  └─────────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────────┘
                    │
                    ▼
    ┌────────────────────────────────────┐
    │   High Priority Compute Stream     │
    │   MLP F (重计算)                    │
    └────────────────────────────────────┘
```

---

## Slide 14: 实验设置

### 模型配置

| 参数 | 3B | 9B | 27B |
|------|-----|-----|-----|
| **Active Params** | 612M | 1.66B | 4.14B |
| **Total Params** | 2.97B | 9.18B | 27.0B |
| **Layers** | 12 | 24 | 61 |
| **Dimension** | 896 | 1280 | 1536 |
| **Expansion Rate** $n$ | 4 | 4 | 4 |
| **Sinkhorn Iterations** | 20 | 20 | 20 |

### 训练配置
- **Architecture**: DeepSeek-V3 based MoE
- **Optimizer**: AdamW (0.9, 0.95)
- **Sequence Length**: 4096
- **Batch Size**: 2560 tokens

---

## Slide 15: 实验结果 - 训练稳定性

### Loss 差距 (vs Baseline)

```
Absolute Loss Gap
   │
 0.00┼──────────────────────────── Baseline
-0.02┼      ╱◄── HC (不稳定)
-0.04┼─────╱
-0.06┼───╱──────────── mHC (稳定)
-0.08┼╱
    0┼────┼────────────┼─────────
      0  10k          30k       50k Steps
```

### 梯度范数对比

```
Grad Norm
   │
0.20┼    ╭──╮        HC 梯度震荡
0.15┼    │  │      ─────────
0.10┼────┤  ├────── Baseline & mHC 稳定
0.05┼    │  │
0.00┼────┴──┴───────────────────
```

---

## Slide 16: 实验结果 - 下游性能

### 27B 模型零样本评估

| 基准 | Metric | Baseline | HC | mHC |
|------|--------|----------|-----|-----|
| **BBH** | EM | 43.8 | 48.9 | **51.0** (+2.1 vs HC) |
| **DROP** | F1 | 47.0 | 51.6 | **53.9** (+2.3 vs HC) |
| **GSM8K** | EM | 46.7 | 53.2 | **53.8** (+0.6) |
| **HellaSwag** | Acc | 73.7 | 74.3 | **74.7** (+0.4) |
| **MATH** | EM | 22.0 | 26.4 | 26.0 |
| **MMLU** | Acc | 59.0 | 63.0 | **63.4** (+0.4) |
| **PIQA** | Acc | 78.5 | 79.9 | **80.5** (+0.6) |
| **TriviaQA** | EM | 54.3 | 56.3 | **57.6** (+1.3) |

**关键发现**：mHC 在推理任务 (BBH, DROP) 上显著提升

---

## Slide 17: 实验结果 - 可扩展性

### 计算缩放曲线

```
Relative Loss Ratio (%)
   │
101.0┼─╱──────────── Baseline
100.5┼╱
100.0┼╱       ◄─────── mHC
 99.5┼╱     ╱
 99.0┼╱────╱─────────
   0└┬────┴─────
     0    2    4  ×10²¹ FLOPs
      (3B→27B)
```

**观察**：mHC 优势在不同计算预算下保持稳定

### Token 缩放曲线 (3B 模型)

训练过程中 loss 持续下降，mHC 始终保持约 1-2% 的优势

---

## Slide 18: 实验结果 - 信号传播可视化

### HC vs mHC 映射对比

```
HC (单层映射)
   │  5.43  4.43  4.43  4.43 18.73  ◄ 行和 (信号增益)
   │[ 0.94 -0.07 -0.05  0.02  0.84 ]
   │[-5.58 -3.74 -5.71 -6.60 -21.64]
   │  │    │    │    │    │
    ◄┴────┴────┴────┴────┴────► 列和 (梯度增益)

mHC (双随机约束)
   │[1.00 0.67 0.09 0.03 0.22]  ◄ 行和 ≈ 1
   │[0.96 0.06 0.05 -0.03 0.87]
   │[0.83 0.73 0.66 0.75 0.00]
   │  │    │    │    │    │
    ◄┴────┴────┴────┴────┴────► 列和 ≈ 1
     1.00 1.00 1.00 1.00 1.00
```

### 复合映射最大增益对比

| 方法 | 最大增益 | 稳定性 |
|------|----------|--------|
| HC | ~3000 | ❌ 不稳定 |
| mHC | ~1.6 | ✅ 稳定 |

---

## Slide 19: 系统性能

### 时间开销

**扩展率 n=4 时：仅 +6.7% 训练时间开销**

### 内存访问优化成果

| 操作 | 优化前读取 | 优化后读取 | 优化前写入 | 优化后写入 |
|------|-----------|-----------|-----------|-----------|
| 残差融合 | $(3n+1)C$ | $(n+1)C$ | $3nC$ | $nC$ |

### 关键优化技术
- Kernel Fusion (减少 40% I/O)
- 混合精度计算 (TileLang)
- 选择性重计算 (优化显存峰值)
- DualPipe 通信重叠

---

## Slide 20: 结论与展望

### 核心贡献总结

| 贡献 | 描述 |
|------|------|
| **理论创新** | 流形约束恢复恒等映射性质 |
| **工程实践** | 仅 6.7% 开销实现大模型训练 |
| **性能提升** | 在 8 个基准上超越 baseline 和 HC |

### 未来研究方向

1. **其他流形约束**探索
   - 正交矩阵约束
   - 稀疏双随机约束

2. **不同几何约束**对稳定性 - 可塑性权衡的影响

3. **宏观架构设计**的进一步探索
   - 拓扑结构与表示学习的关系
   - 下一代基础模型架构演进

---

## Slide 21: Q&A

### 感谢聆听！

**联系信息**
- Email: xie.zhenda@deepseek.com
- Paper: arXiv:2512.24880
- Authors: DeepSeek-AI

---

## Appendix A: 关键公式汇总

### 标准残差连接
$$x^{l+1} = x^l + \mathcal{F}(x^l, W^l)$$

### Hyper-Connections
$$x^{l+1} = H^l_{\text{res}} x^l + H^{l\top}_{\text{post}} \mathcal{F}(H^l_{\text{pre}} x^l, W^l)$$

### Manifold-Constrained HC
$$H^l_{\text{res}} \in \mathcal{M}^{\text{res}} = \{ H \in \mathbb{R}^{n\times n} \mid H\mathbf{1}=\mathbf{1}, \mathbf{1}^\top H = \mathbf{1}^\top, H\geq0 \}$$

### Sinkhorn-Knopp 迭代
$$M^{(t)} = T_r T_c (M^{(t-1)})$$

### 重计算块大小最优解
$$L_r^* \approx \sqrt{\frac{nL}{n+2}}$$

---

## Appendix B: 架构图示符号说明

| 符号 | 含义 |
|------|------|
| (F) | Forward pass |
| (B) | Backward pass |
| (W) | Weight gradient |
| PP Send Recv | Pipeline Parallelism 通信 |
| MLP | Multi-Layer Perceptron |
| ATTN | Attention |
| COMBINE | 特征组合操作 |
| DISPATCH | 特征分发操作 |

---

*报告生成于 2025, mHC Paper*

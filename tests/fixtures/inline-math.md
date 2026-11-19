# Inline math

Prose before $x^2+y_1$ and between $\alpha+\beta$ and after both formulas.

Parenthesis math before \(x_1+\frac{a*b}{c}\) and after \(\alpha+\beta\).
Mixed delimiters $x$ and \(y\), adjacent atoms \(a\)\(b\), and Unicode 日本 \(x^2\) prose.
Markdown-sensitive TeX \(\text{[a] (b) *c*}\) remains one formula.
Code `\(x\)`, escaped \\(literal\\), and unmatched \(text remain ordinary Markdown.

A fraction $\dfrac{a+b}{c+d}$ shares a baseline with this sentence.
A matrix $\begin{matrix}a&b\\c&d\\e&f\end{matrix}$ makes the row taller.
Cases $f(x)=\begin{cases}x&x>0\\-x&x\leq0\end{cases}$ remain one atomic formula.

This paragraph has
several source lines with $x^2$ and
more prose beside $y_1$ for reflow and resize checks.

$$
\int_0^1 x^2\,dx=\frac13
$$

Literal currency: $5 and $10 total. Escaped dollar: \$5.
Inline code `$x^2$` is not typeset. An unmatched dollar stays literal: $

An unsupported formula $\notARealCommand{x}$ must remain readable.

> Nested math $x^2$ currently stays literal.

- List math $x^2$ currently stays literal.

| Table math |
| --- |
| $x^2$ |

```text
$x^2$ and $$y^2$$ are code here.
```

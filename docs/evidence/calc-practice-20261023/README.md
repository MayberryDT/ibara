# Calc practice: saved output

[Download the original ODS](ibara-practice-expenses.ods) from the [copyable Calc task](https://gist.github.com/MayberryDT/3a551224ef2bdb249446f87536072950).

This is the original file produced by one internal ibara task on 23 October 2026. The agent entered fictional expenses through native Calc input, saved the file, closed and reopened it, and collected a byte-identical copy back to the working computer. No real financial records were used.

Open it in LibreOffice Calc and inspect these cells:

| Cell | Content |
|---|---|
| A1 / B1 | Item / Amount |
| A2 / B2 | Notebook / numeric 6.5 |
| A3 / B3 | Pens / numeric 4 |
| A4 / B4 | Cable / numeric 8 |
| A5 / B5 | Total / SUM of B2:B4, result 18.5 |

The saved ODS stores B5 as `of:=SUM([.B2:.B4])`, with cached value `18.5`. The sheet was also visually checked after reopening. A displayed value of 18.5 is the same number as 18.50.

Original file: 9,623 bytes. SHA-256:

```text
27025433e01e1317906f950b22a8e7dffced5d6a725c42d3ee84baa5a13b7b3d
```

Your own saved file can have different metadata and a different hash. Check its cells and formula.

Test environment: an Omarchy Dell OptiPlex with ibara 0.3.1-1, LibreOffice 26.8.0.3, Hyprland 0.56.2 and Tailscale 1.102.3; the client was 0.3.1-dev.local. The task ended with its application windows closed and control released.

This is an internal example, not an external-user report or a guarantee that every agent will complete the exercise. The file and walkthrough were prepared by Tyler's AI assistant for ibara.

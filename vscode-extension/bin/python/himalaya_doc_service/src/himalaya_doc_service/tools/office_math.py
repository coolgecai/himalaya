"""Small LaTeX-to-Office-Math bridge for PPTX generation.

PowerPoint stores editable equations as Office Math Markup Language (OMML)
inside DrawingML paragraphs. This module intentionally covers the common
formula subset used in generated academic decks and leaves image rendering as
the high-fidelity fallback for unusual LaTeX.
"""

from __future__ import annotations

import html
import re


M_NS = "http://schemas.openxmlformats.org/officeDocument/2006/math"
A_NS = "http://schemas.openxmlformats.org/drawingml/2006/main"
A14_NS = "http://schemas.microsoft.com/office/drawing/2010/main"


COMMAND_SYMBOLS = {
    "alpha": "\u03b1",
    "beta": "\u03b2",
    "gamma": "\u03b3",
    "delta": "\u03b4",
    "epsilon": "\u03b5",
    "varepsilon": "\u03b5",
    "zeta": "\u03b6",
    "eta": "\u03b7",
    "theta": "\u03b8",
    "vartheta": "\u03d1",
    "iota": "\u03b9",
    "kappa": "\u03ba",
    "lambda": "\u03bb",
    "mu": "\u03bc",
    "nu": "\u03bd",
    "xi": "\u03be",
    "pi": "\u03c0",
    "rho": "\u03c1",
    "sigma": "\u03c3",
    "tau": "\u03c4",
    "upsilon": "\u03c5",
    "phi": "\u03c6",
    "varphi": "\u03d5",
    "chi": "\u03c7",
    "psi": "\u03c8",
    "omega": "\u03c9",
    "Gamma": "\u0393",
    "Delta": "\u0394",
    "Theta": "\u0398",
    "Lambda": "\u039b",
    "Xi": "\u039e",
    "Pi": "\u03a0",
    "Sigma": "\u03a3",
    "Phi": "\u03a6",
    "Psi": "\u03a8",
    "Omega": "\u03a9",
    "sum": "\u2211",
    "prod": "\u220f",
    "int": "\u222b",
    "infty": "\u221e",
    "partial": "\u2202",
    "nabla": "\u2207",
    "times": "\u00d7",
    "cdot": "\u00b7",
    "le": "\u2264",
    "leq": "\u2264",
    "ge": "\u2265",
    "geq": "\u2265",
    "neq": "\u2260",
    "approx": "\u2248",
    "sim": "\u223c",
    "in": "\u2208",
    "notin": "\u2209",
    "subset": "\u2282",
    "subseteq": "\u2286",
    "cup": "\u222a",
    "cap": "\u2229",
    "rightarrow": "\u2192",
    "to": "\u2192",
    "leftarrow": "\u2190",
    "Rightarrow": "\u21d2",
    "Leftarrow": "\u21d0",
    "pm": "\u00b1",
    "mp": "\u2213",
    "div": "\u00f7",
    "ldots": "\u2026",
    "cdots": "\u22ef",
}

TEXT_COMMANDS = {
    "mathrm",
    "mathbf",
    "mathit",
    "mathsf",
    "mathtt",
    "mathcal",
    "operatorname",
    "text",
    "textrm",
}

IGNORED_COMMANDS = {
    "left",
    "right",
    "big",
    "Big",
    "bigg",
    "Bigg",
    "!",
    ",",
    ";",
    ":",
    "quad",
    "qquad",
}


def latex_to_powerpoint_math_paragraph(latex: str, font_size_pt: int = 20) -> str:
    """Return a DrawingML paragraph containing editable OMML equation XML."""

    expr = normalize_latex(latex)
    parser = _LatexParser(expr)
    body = parser.parse()
    if not body:
        raise ValueError("empty formula")
    size = max(800, int(font_size_pt * 100))
    return (
        f'<a:p xmlns:a="{A_NS}" xmlns:a14="{A14_NS}" xmlns:m="{M_NS}">'
        '<a:pPr algn="ctr"/>'
        '<a14:m>'
        '<m:oMathPara>'
        '<m:oMathParaPr><m:jc m:val="center"/></m:oMathParaPr>'
        f'<m:oMath>{body}</m:oMath>'
        '</m:oMathPara>'
        '</a14:m>'
        f'<a:endParaRPr lang="zh-CN" sz="{size}"/>'
        '</a:p>'
    )


def normalize_latex(latex: str) -> str:
    expr = (latex or "").strip()
    expr = re.sub(r"^\s*\$\$(.*)\$\$\s*$", r"\1", expr, flags=re.DOTALL)
    expr = re.sub(r"^\s*\$(.*)\$\s*$", r"\1", expr, flags=re.DOTALL)
    expr = re.sub(r"^\\\[(.*)\\\]$", r"\1", expr, flags=re.DOTALL)
    for env in ("equation", "equation*", "align", "align*", "aligned", "gather", "gather*"):
        expr = expr.replace(f"\\begin{{{env}}}", "").replace(f"\\end{{{env}}}", "")
    expr = expr.replace("&=", "=").replace("&", "")
    return " ".join(expr.split()) if "\n" not in expr else expr.strip()


def latex_to_plain_math_text(latex: str) -> str:
    parser = _PlainLatexParser(normalize_latex(latex))
    return parser.parse().strip()


class _LatexParser:
    def __init__(self, expr: str):
        self.expr = expr
        self.i = 0

    def parse(self) -> str:
        return self._parse_until(None)

    def _parse_until(self, end_char: str | None) -> str:
        nodes: list[str] = []
        while self.i < len(self.expr):
            if end_char and self.expr[self.i] == end_char:
                self.i += 1
                break
            if self.expr[self.i].isspace():
                self.i += 1
                nodes.append(_run(" "))
                continue
            atom = self._parse_atom()
            if not atom:
                continue
            sub = None
            sup = None
            while self._peek() in ("_", "^"):
                op = self.expr[self.i]
                self.i += 1
                arg = self._parse_script_arg()
                if op == "_":
                    sub = arg
                else:
                    sup = arg
            if sub and sup:
                atom = _subsup(atom, sub, sup)
            elif sub:
                atom = _sub(atom, sub)
            elif sup:
                atom = _sup(atom, sup)
            nodes.append(atom)
        return "".join(nodes)

    def _parse_atom(self) -> str:
        if self.i >= len(self.expr):
            return ""
        ch = self.expr[self.i]
        if ch == "{":
            self.i += 1
            return self._parse_until("}")
        if ch == "}":
            return ""
        if ch == "\\":
            return self._parse_command()
        if ch in "_^":
            return ""

        start = self.i
        while self.i < len(self.expr) and self.expr[self.i] not in "\\{}_^":
            if self.expr[self.i].isspace():
                break
            self.i += 1
        return _run(self.expr[start:self.i])

    def _parse_command(self) -> str:
        self.i += 1
        if self.i >= len(self.expr):
            return _run("\\")
        if self.expr[self.i].isalpha():
            start = self.i
            while self.i < len(self.expr) and self.expr[self.i].isalpha():
                self.i += 1
            command = self.expr[start:self.i]
        else:
            command = self.expr[self.i]
            self.i += 1

        if command in IGNORED_COMMANDS:
            return ""
        if command in ("frac", "dfrac", "tfrac"):
            return _fraction(self._parse_required_group(), self._parse_required_group())
        if command == "sqrt":
            if self._peek() == "[":
                self._skip_optional_group()
            return _radical(self._parse_required_group())
        if command in TEXT_COMMANDS:
            return _run(_PlainLatexParser(self._group_source()).parse())
        if command in COMMAND_SYMBOLS:
            return _run(COMMAND_SYMBOLS[command])
        if command in ("{", "}"):
            return _run(command)
        return _run(command)

    def _parse_script_arg(self) -> str:
        self._skip_spaces()
        if self._peek() == "{":
            self.i += 1
            return self._parse_until("}")
        return self._parse_atom()

    def _parse_required_group(self) -> str:
        self._skip_spaces()
        if self._peek() == "{":
            self.i += 1
            return self._parse_until("}")
        return self._parse_atom()

    def _group_source(self) -> str:
        self._skip_spaces()
        if self._peek() != "{":
            return ""
        self.i += 1
        start = self.i
        depth = 1
        while self.i < len(self.expr) and depth:
            ch = self.expr[self.i]
            if ch == "{":
                depth += 1
            elif ch == "}":
                depth -= 1
            self.i += 1
        end = self.i - 1 if depth == 0 else self.i
        return self.expr[start:end]

    def _skip_optional_group(self) -> None:
        if self._peek() != "[":
            return
        depth = 1
        self.i += 1
        while self.i < len(self.expr) and depth:
            if self.expr[self.i] == "[":
                depth += 1
            elif self.expr[self.i] == "]":
                depth -= 1
            self.i += 1

    def _skip_spaces(self) -> None:
        while self.i < len(self.expr) and self.expr[self.i].isspace():
            self.i += 1

    def _peek(self) -> str | None:
        return self.expr[self.i] if self.i < len(self.expr) else None


class _PlainLatexParser:
    def __init__(self, expr: str):
        self.expr = expr
        self.i = 0

    def parse(self) -> str:
        parts: list[str] = []
        while self.i < len(self.expr):
            ch = self.expr[self.i]
            if ch == "\\":
                parts.append(self._command())
            elif ch in "{}":
                self.i += 1
            elif ch in "_^":
                parts.append(ch)
                self.i += 1
            else:
                parts.append(ch)
                self.i += 1
        return "".join(parts)

    def _command(self) -> str:
        self.i += 1
        if self.i >= len(self.expr):
            return "\\"
        if self.expr[self.i].isalpha():
            start = self.i
            while self.i < len(self.expr) and self.expr[self.i].isalpha():
                self.i += 1
            command = self.expr[start:self.i]
        else:
            command = self.expr[self.i]
            self.i += 1
        return COMMAND_SYMBOLS.get(command, "" if command in IGNORED_COMMANDS else command)


def _run(text: str) -> str:
    if not text:
        return ""
    return f"<m:r><m:t>{html.escape(text, quote=False)}</m:t></m:r>"


def _fraction(num: str, den: str) -> str:
    return f"<m:f><m:num>{num}</m:num><m:den>{den}</m:den></m:f>"


def _radical(body: str) -> str:
    return f"<m:rad><m:radPr><m:degHide m:val=\"1\"/></m:radPr><m:e>{body}</m:e></m:rad>"


def _sub(base: str, sub: str) -> str:
    return f"<m:sSub><m:e>{base}</m:e><m:sub>{sub}</m:sub></m:sSub>"


def _sup(base: str, sup: str) -> str:
    return f"<m:sSup><m:e>{base}</m:e><m:sup>{sup}</m:sup></m:sSup>"


def _subsup(base: str, sub: str, sup: str) -> str:
    return f"<m:sSubSup><m:e>{base}</m:e><m:sub>{sub}</m:sub><m:sup>{sup}</m:sup></m:sSubSup>"

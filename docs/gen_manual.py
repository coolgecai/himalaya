# -*- coding: utf-8 -*-
from fpdf import FPDF
from fpdf.enums import XPos, YPos
import os

FONT_DIR = "C:/Windows/Fonts"
MSYH    = os.path.join(FONT_DIR, "msyh.ttc")
MSYHBD  = os.path.join(FONT_DIR, "msyhbd.ttc")
COURIER = os.path.join(FONT_DIR, "cour.ttf")
COURBD  = os.path.join(FONT_DIR, "courbd.ttf")

class Manual(FPDF):
    def __init__(self):
        super().__init__(orientation="P", unit="mm", format="A4")
        self.add_font("msyh",    "",  MSYH,    uni=True)
        self.add_font("msyh",    "B", MSYHBD,  uni=True)
        self.add_font("courier", "",  COURIER, uni=True)
        self.add_font("courier", "B", COURBD,  uni=True)
        self.set_auto_page_break(auto=True, margin=20)
        self.set_margins(20, 20, 20)
        self._toc = []

    def body_font(self, bold=False, size=10.5):
        self.set_font("msyh", "B" if bold else "", size)

    def code_font(self, size=9):
        self.set_font("courier", "", size)

    def h(self, level, text):
        sizes = {1: 20, 2: 15, 3: 12}
        gaps  = {1: (10, 4), 2: (8, 3), 3: (6, 2)}
        self.ln(gaps[level][0])
        self.body_font(bold=True, size=sizes[level])
        self.set_text_color(30, 30, 30)
        self.multi_cell(0, sizes[level]*0.55, text, new_x=XPos.LMARGIN, new_y=YPos.NEXT)
        self.ln(gaps[level][1])
        self._toc.append((level, text, self.page_no()))
        self.body_font(size=10.5)
        self.set_text_color(0, 0, 0)

    def p(self, text, indent=0):
        self.body_font(size=10.5)
        if indent:
            self.set_x(self.get_x() + indent)
        self.multi_cell(0, 6, text, new_x=XPos.LMARGIN, new_y=YPos.NEXT)
        self.ln(1)

    def bullet(self, text, indent=6):
        self.body_font(size=10.5)
        self.set_x(self.l_margin + indent)
        self.cell(4, 6, "\u2022")
        self.multi_cell(0, 6, text, new_x=XPos.LMARGIN, new_y=YPos.NEXT)

    def code_block(self, lines):
        self.ln(1)
        self.set_fill_color(245, 245, 245)
        self.set_draw_color(200, 200, 200)
        self.code_font(size=9)
        for line in lines:
            self.set_x(self.l_margin)
            self.cell(0, 5.5, line, fill=True, border=0,
                      new_x=XPos.LMARGIN, new_y=YPos.NEXT)
        self.ln(2)
        self.body_font(size=10.5)

    def note(self, text):
        self.set_fill_color(255, 248, 220)
        self.set_draw_color(220, 180, 60)
        self.body_font(size=10)
        self.set_x(self.l_margin)
        self.multi_cell(0, 6, "  \u26a0  " + text, border=1, fill=True,
                        new_x=XPos.LMARGIN, new_y=YPos.NEXT)
        self.ln(2)
        self.body_font(size=10.5)

    def tip(self, text):
        self.set_fill_color(235, 248, 235)
        self.set_draw_color(100, 180, 100)
        self.body_font(size=10)
        self.set_x(self.l_margin)
        self.multi_cell(0, 6, "  \u2713  " + text, border=1, fill=True,
                        new_x=XPos.LMARGIN, new_y=YPos.NEXT)
        self.ln(2)
        self.body_font(size=10.5)

    def table(self, headers, rows, col_widths=None):
        w = col_widths or [170 // len(headers)] * len(headers)
        self.body_font(bold=True, size=10)
        self.set_fill_color(50, 50, 50)
        self.set_text_color(255, 255, 255)
        for i, hdr in enumerate(headers):
            self.cell(w[i], 7, hdr, border=1, fill=True)
        self.ln()
        self.body_font(size=9.5)
        self.set_text_color(0, 0, 0)
        for ri, row in enumerate(rows):
            self.set_fill_color(250, 250, 250) if ri % 2 == 0 else self.set_fill_color(255, 255, 255)
            for i, cell in enumerate(row):
                self.cell(w[i], 6.5, cell, border=1, fill=True)
            self.ln()
        self.ln(3)
        self.body_font(size=10.5)

    def hline(self):
        self.set_draw_color(180, 180, 180)
        self.line(self.l_margin, self.get_y(), self.w - self.r_margin, self.get_y())
        self.ln(3)

    def header(self):
        if self.page_no() == 1:
            return
        self.body_font(size=8.5)
        self.set_text_color(120, 120, 120)
        self.cell(0, 8, "Claw Code  \u7528\u6237\u624b\u518c  v0.1.0", align="L")
        self.set_y(self.t_margin)

    def footer(self):
        if self.page_no() == 1:
            return
        self.set_y(-15)
        self.body_font(size=8.5)
        self.set_text_color(120, 120, 120)
        self.cell(0, 8, f"\u7b2c {self.page_no()} \u9875", align="C")

    def cover(self):
        self.add_page()
        self.set_fill_color(20, 20, 20)
        self.rect(0, 0, self.w, self.h, "F")
        self.set_y(55)
        self.set_font("courier", "", 13)
        self.set_text_color(180, 60, 60)
        for line in [
            "   ____  _                   ____          _",
            "  / ___|| |  __ _ __      __ / ___|___   __| | ___",
            " | |    | | / _` |\\ \\ /\\ / /| |   / _ \\ / _` |/ _ \\",
            " | |___ | || (_| | \\ V  V / | |__| (_) | (_| |  __/",
            "  \\____||_| \\__,_|  \\_/\\_/   \\____\\___/ \\__,_|\\___|",
        ]:
            self.cell(0, 6.5, line, new_x=XPos.LMARGIN, new_y=YPos.NEXT, align="C")
        self.ln(12)
        self.body_font(bold=True, size=28)
        self.set_text_color(255, 255, 255)
        self.cell(0, 14, "\u7528 \u6237 \u624b \u518c", align="C", new_x=XPos.LMARGIN, new_y=YPos.NEXT)
        self.ln(4)
        self.body_font(size=13)
        self.set_text_color(160, 160, 160)
        self.cell(0, 8, "\u7248\u672c  0.1.0  \u00b7  Windows x64", align="C",
                  new_x=XPos.LMARGIN, new_y=YPos.NEXT)
        self.ln(60)
        self.body_font(size=10)
        self.set_text_color(100, 100, 100)
        self.cell(0, 6, "\u6784\u5efa\u65e5\u671f\uff1a2026-04-14", align="C",
                  new_x=XPos.LMARGIN, new_y=YPos.NEXT)
        self.cell(0, 6, "\u76ee\u6807\u5e73\u53f0\uff1ax86_64-pc-windows-msvc", align="C",
                  new_x=XPos.LMARGIN, new_y=YPos.NEXT)
        self.set_text_color(0, 0, 0)

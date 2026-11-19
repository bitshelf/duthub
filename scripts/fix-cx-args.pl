#!/usr/bin/perl
# 给 w.select_field(...) / w.combobox(...) 的调用尾部插入 ", cx"（若无）。
use strict; use warnings;

for my $file (@ARGV) {
    open my $fh, '<', $file or die $!;
    my $src = do { local $/; <$fh> };
    close $fh;
    my $out = '';
    my $pos = 0;
    my $changed = 0;
    while ($src =~ /w\.(select_field|combobox)\(/g) {
        my $start = $-[0] + length($&);
        # 从 start 开始做括号平衡扫描
        my $depth = 1;
        my $i = $start;
        my $in_str = 0; my $str_ch = '';
        while ($i < length($src) && $depth > 0) {
            my $c = substr($src, $i, 1);
            if ($in_str) {
                if ($c eq '\\') { $i += 2; next; }
                $in_str = 0 if $c eq $str_ch;
            } else {
                if ($c eq '"' || $c eq "'") { $in_str = 1; $str_ch = $c; }
                elsif ($c eq '(') { $depth++; }
                elsif ($c eq ')') { $depth--; }
            }
            $i++;
        }
        my $close = $i - 1;   # 指向匹配的 ')'
        my $before = substr($src, $close - 1, 1);
        if ($before ne 'x') {   # 尾部不是 ", cx"
            # 找最后一个非空白
            my $j = $close - 1;
            $j-- while $j > 0 && substr($src, $j, 1) =~ /\s/;
            $out .= substr($src, $pos, $j + 1 - $pos) . ", cx" . substr($src, $j + 1, $close - $j - 1) . ")";
            $changed++;
        } else {
            $out .= substr($src, $pos, $close + 1 - $pos);
        }
        $pos = $close + 1;
    }
    $out .= substr($src, $pos);
    if ($changed) {
        open my $w, '>', $file or die $!;
        print $w $out;
        close $w;
    }
    print "$file: +cx ×$changed\n";
}

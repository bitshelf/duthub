#!/usr/bin/perl
# 从 duthub.html 提取 UI 行为规格的机器可核对部分：
#   1. i18n 字符串表（zh-CN / en-US）→ docs/ui-spec/i18n.md
#   2. 主题 token（:root CSS 变量，dark/light）→ docs/ui-spec/theme.md
# 核对规则：zh 与 en 键集合必须一致；输出条目数写入文档头部供人工/脚本复核。
use strict;
use warnings;

my $root = $0 =~ s{/[^/]*$}{}r eq $0 ? '.' : $0 =~ s{/[^/]*$}{}r;
my $html_file = "$root/../duthub.html";
my $out_dir = "$root/../docs/ui-spec";

open my $fh, '<', $html_file or die "cannot open $html_file: $!";
my @lines = <$fh>;
close $fh;

sub extract_kv {
    my ($body) = @_;
    my %out;
    while ($body =~ /([A-Za-z0-9]+):'((?:\\.|[^'\\])*)'/g) {
        $out{$1} = $2;
    }
    return \%out;
}

my ($zh_body, $en_body);
for my $line (@lines) {
    # zh 行尾 "...'},", en 行尾 "...'}};"：循环剥离尾部结构字符 } ; ,
    if ($line =~ /^\s*'zh-CN':\{(.+)$/) { $zh_body = strip_close($1); }
    elsif ($line =~ /^\s*'en-US':\{(.+)$/) { $en_body = strip_close($1); }
}
die "未找到 i18n 块" unless defined $zh_body && defined $en_body;

sub strip_close {
    my $s = shift;
    $s =~ s/[};,]\s*$// while $s =~ /[};,]\s*$/;
    return $s;
}

my $zh = extract_kv($zh_body);
my $en = extract_kv($en_body);

# 核对 1：zh / en 键集合一致
my @missing_in_en = grep { !exists $en->{$_} } sort keys %$zh;
my @missing_in_zh = grep { !exists $zh->{$_} } sort keys %$en;
die "i18n 键不一致：en 缺 [@missing_in_en]，zh 缺 [@missing_in_zh]"
    if @missing_in_en || @missing_in_zh;

# 核对 2：标记里用到的 data-i18n 键必须都在表里
my %used;
for my $line (@lines) {
    while ($line =~ /data-i18n="([A-Za-z0-9]+)"/g) { $used{$1} = 1; }
}
my @used_missing = grep { !exists $zh->{$_} } sort keys %used;

mkdir $out_dir unless -d $out_dir;

# i18n.md
{
    open my $out, '>', "$out_dir/i18n.md" or die $!;
    printf $out "# i18n 字符串规格（自动提取）\n\n" .
        "> 来源：`duthub.html` 内嵌 `const i18n` 表；由 `scripts/extract-ui-spec.pl` 生成。\n" .
        "> 条目数：**zh=%d / en=%d**；标记中 `data-i18n` 引用的去重键数：%d；表内缺失：%s\n\n" .
        "| key | zh-CN | en-US |\n|---|---|---|\n",
        scalar(keys %$zh), scalar(keys %$en), scalar(keys %used),
        @used_missing ? join(', ', @used_missing) : '无';
    for my $k (sort keys %$zh) {
        (my $z = $zh->{$k}) =~ s/\|/\\|/g;
        (my $e = $en->{$k}) =~ s/\|/\\|/g;
        print $out "| $k | $z | $e |\n";
    }
    close $out;
}

# theme.md
{
    my ($dark, $light);
    for my $line (@lines) {
        $dark = $line if !defined $dark && $line =~ /^:root\{--bg:/;
        $light = $line if !defined $light && $line =~ /^:root\[data-theme=light\]/;
    }
    die "未找到主题 token" unless defined $dark && defined $light;
    open my $out, '>', "$out_dir/theme.md" or die $!;
    print $out "# 主题 token 规格（自动提取）\n\n" .
        "> GPUI 深色/浅色主题的移植对照；变量名即语义。由 `scripts/extract-ui-spec.pl` 生成。\n\n" .
        "## Dark（默认）\n\n```css\n$dark```\n\n" .
        "## Light\n\n```css\n$light```\n";
    close $out;
}

printf "OK: i18n zh=%d en=%d used=%d %s\n", scalar(keys %$zh), scalar(keys %$en),
    scalar(keys %used), @used_missing ? ("MISSING: @used_missing") : "引用完整";

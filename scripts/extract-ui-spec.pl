#!/usr/bin/perl
# Extract machine-checkable UI specifications from the archived duthub prototype.
#   Write localized strings for zh-CN and en-US to docs/ui-spec/i18n.md.
#   Write the light and dark theme tokens from CSS variables to docs/ui-spec/theme.md.
# Validate that the locales have identical keys and include counts for reviewers.
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
    # Strip trailing object punctuation from both locale records.
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

# Verify that both locale tables use identical keys.
my @missing_in_en = grep { !exists $en->{$_} } sort keys %$zh;
my @missing_in_zh = grep { !exists $zh->{$_} } sort keys %$en;
die "i18n 键不一致：en 缺 [@missing_in_en]，zh 缺 [@missing_in_zh]"
    if @missing_in_en || @missing_in_zh;

# Verify that each data-i18n key used by the markup exists.
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

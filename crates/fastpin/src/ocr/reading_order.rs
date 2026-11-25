//! Group detected boxes into reading order. Ported from C# `OcrReadingOrder`.
//!
//! The detector emits boxes in score order, so a raw join interleaves columns
//! and reads as noise. Splitting by horizontal whitespace into columns first,
//! then grouping each column by vertical band, is what keeps side-by-side text
//! from being read as "left1 right1 left2 right2".

/// One detected box: the recognized text plus its detection polygon.
pub struct Block {
    pub text: String,
    pub poly: Vec<(f64, f64)>,
}

impl Block {
    fn centroid(&self) -> (f64, f64) {
        if self.poly.is_empty() {
            return (0.0, 0.0);
        }
        let n = self.poly.len() as f64;
        let (sx, sy) = self
            .poly
            .iter()
            .fold((0.0, 0.0), |(ax, ay), (x, y)| (ax + x, ay + y));
        (sx / n, sy / n)
    }

    fn height(&self) -> f64 {
        let ys = || self.poly.iter().map(|(_, y)| *y);
        match (
            ys().fold(f64::NEG_INFINITY, f64::max),
            ys().fold(f64::INFINITY, f64::min),
        ) {
            (max, min) if max.is_finite() && min.is_finite() => max - min,
            _ => 0.0,
        }
    }

    /// x 方向的包围区间，用来找列与列之间的竖直空白带。
    fn x_range(&self) -> (f64, f64) {
        let xs = || self.poly.iter().map(|(x, _)| *x);
        match (
            xs().fold(f64::INFINITY, f64::min),
            xs().fold(f64::NEG_INFINITY, f64::max),
        ) {
            (min, max) if min.is_finite() && max.is_finite() => (min, max),
            _ => (0.0, 0.0),
        }
    }

    /// y 方向的包围区间，用来判断候选两栏是否真的并排而非上下错位。
    fn y_range(&self) -> (f64, f64) {
        let ys = || self.poly.iter().map(|(_, y)| *y);
        match (
            ys().fold(f64::INFINITY, f64::min),
            ys().fold(f64::NEG_INFINITY, f64::max),
        ) {
            (min, max) if min.is_finite() && max.is_finite() => (min, max),
            _ => (0.0, 0.0),
        }
    }
}

pub fn to_plain_text(blocks: &[Block]) -> String {
    group_lines(blocks)
        .iter()
        .map(|line| {
            line.iter()
                .map(|b| b.text.as_str())
                .filter(|t| !t.trim().is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn group_lines(blocks: &[Block]) -> Vec<Vec<&Block>> {
    match blocks.len() {
        0 => return Vec::new(),
        1 => return vec![vec![&blocks[0]]],
        _ => {}
    }

    // 8px floor keeps tiny detections from each becoming their own line.
    let threshold = (median_height(blocks) * 0.6).max(8.0);
    let columns = split_columns(blocks, threshold);

    let mut lines: Vec<Vec<&Block>> = Vec::new();
    // 每列内部沿用原来的分带逻辑；单栏时 columns 只有一列，行为与旧代码一致。
    for column in columns {
        let column_start = lines.len();
        let mut ordered = column;
        ordered.sort_by(|a, b| {
            let (ax, ay) = a.centroid();
            let (bx, by) = b.centroid();
            ay.total_cmp(&by).then(ax.total_cmp(&bx))
        });
        for block in ordered {
            // Compare against the running mean of the open line, not its first box,
            // so a slightly rising row of boxes stays one line. A new column
            // always opens a fresh line even if its first row shares a y with
            // the previous column's last row.
            let same_line = lines.len() > column_start
                && lines.last().is_some_and(|line| {
                    let mean = line.iter().map(|b| b.centroid().1).sum::<f64>() / line.len() as f64;
                    (block.centroid().1 - mean).abs() <= threshold
                });
            if same_line {
                lines.last_mut().expect("checked above").push(block);
            } else {
                lines.push(vec![block]);
            }
        }
    }

    for line in &mut lines {
        line.sort_by(|a, b| a.centroid().0.total_cmp(&b.centroid().0));
    }
    lines
}

/// 按 x 间断聚类分列：两栏之间是一整条竖直空白，同一行的相邻词之间只是
/// 窄字距。只有间断明显大于行高、两侧各自至少有两行、且两侧 y 包围区间
/// 实质重叠时才切列，避免把一行的几个词或上下错位的块误判成两栏。
fn split_columns(blocks: &[Block], line_threshold: f64) -> Vec<Vec<&Block>> {
    let mut by_x: Vec<&Block> = blocks.iter().collect();
    by_x.sort_by(|a, b| a.x_range().0.total_cmp(&b.x_range().0));

    // 20px 行高下的 30px 词间距仍属于同一栏，所以阈值取行高的两倍；
    // 大字号或图片框会拉偏全局中位行高，钳制后不让空白带无限变宽。
    let gap = (median_height(blocks) * 2.0).clamp(24.0, 80.0);
    let mut columns: Vec<Vec<&Block>> = Vec::new();
    let mut current: Vec<&Block> = Vec::new();
    let mut right = f64::NEG_INFINITY;

    for (i, block) in by_x.iter().enumerate() {
        let (left, block_right) = block.x_range();
        if !current.is_empty()
            && left - right > gap
            && has_two_lines(&current, line_threshold)
            && has_two_lines(&by_x[i..], line_threshold)
            && y_bands_overlap(&current, &by_x[i..])
        {
            // by_x 已按左边界排序，先 push 的总是更靠左的列。
            columns.push(std::mem::take(&mut current));
            right = f64::NEG_INFINITY;
        }
        right = right.max(block_right);
        current.push(*block);
    }
    if !current.is_empty() {
        columns.push(current);
    }
    columns
}

/// 分栏的“至少两行”判定：行距按该侧块高局部估计并钳制，
/// 避免全局中位行高被大字号或图片框带偏。
fn has_two_lines(blocks: &[&Block], fallback: f64) -> bool {
    line_count(blocks, local_line_threshold(blocks).unwrap_or(fallback)) >= 2
}

fn local_line_threshold(blocks: &[&Block]) -> Option<f64> {
    let mut heights: Vec<f64> = blocks
        .iter()
        .map(|b| b.height())
        .filter(|h| *h > 0.0)
        .collect();
    if heights.is_empty() {
        return None;
    }
    heights.sort_by(f64::total_cmp);
    Some((heights[heights.len() / 2] * 0.6).clamp(8.0, 64.0))
}

/// 只有水平空白贯穿两侧 y 跨度才算分栏：右对齐标题压在左对齐正文上方时，
/// 两侧虽然 x 间断很大，但 y 区间上下错位，必须留在一栏里按 y 排序。
fn y_bands_overlap(left: &[&Block], right: &[&Block]) -> bool {
    let (l0, l1) = y_bounds(left);
    let (r0, r1) = y_bounds(right);
    let span_l = l1 - l0;
    let span_r = r1 - r0;
    let overlap = (l1.min(r1) - l0.max(r0)).max(0.0);
    if span_l <= 0.0 || span_r <= 0.0 {
        // 退化成一条线时改用中点比较，避免零跨度让重叠比例失真。
        return ((l0 + l1) - (r0 + r1)).abs() * 0.5 <= 1.0;
    }
    overlap >= span_l.min(span_r) * 0.5
}

fn y_bounds(blocks: &[&Block]) -> (f64, f64) {
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for block in blocks {
        let (y0, y1) = block.y_range();
        min = min.min(y0);
        max = max.max(y1);
    }
    if min.is_finite() && max.is_finite() {
        (min, max)
    } else {
        (0.0, 0.0)
    }
}

/// 统计块在 y 方向分成几带，用来判断一次 x 间断是不是真的分栏。
fn line_count(blocks: &[&Block], threshold: f64) -> usize {
    let mut ys: Vec<f64> = blocks.iter().map(|b| b.centroid().1).collect();
    ys.sort_by(f64::total_cmp);
    let mut count = 0;
    for adjacent in ys.windows(2) {
        if adjacent[1] - adjacent[0] > threshold {
            count += 1;
        }
    }
    if ys.is_empty() {
        0
    } else {
        count + 1
    }
}

fn median_height(blocks: &[Block]) -> f64 {
    let mut heights: Vec<f64> = blocks
        .iter()
        .map(Block::height)
        .filter(|h| *h > 0.0)
        .collect();
    if heights.is_empty() {
        return 16.0;
    }
    heights.sort_by(f64::total_cmp);
    heights[heights.len() / 2]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Axis-aligned box, the shape the detector emits for horizontal text.
    fn block(text: &str, x: f64, y: f64, w: f64, h: f64) -> Block {
        Block {
            text: text.into(),
            poly: vec![(x, y), (x + w, y), (x + w, y + h), (x, y + h)],
        }
    }

    #[test]
    fn empty_and_single_blocks() {
        assert_eq!(to_plain_text(&[]), "");
        assert_eq!(
            to_plain_text(&[block("只有一行", 0.0, 0.0, 100.0, 20.0)]),
            "只有一行"
        );
    }

    #[test]
    fn boxes_on_one_row_join_with_a_space_left_to_right() {
        // Fed in right-to-left to prove the sort runs.
        let blocks = vec![
            block("World", 120.0, 10.0, 80.0, 20.0),
            block("Hello", 10.0, 12.0, 80.0, 20.0),
        ];
        assert_eq!(to_plain_text(&blocks), "Hello World");
    }

    #[test]
    fn two_columns_are_read_top_to_bottom_then_left_to_right() {
        // 左右两栏各两行，故意打乱输入顺序；同一 y 上的左右块不能先被拼成一行。
        let blocks = vec![
            block("右1", 200.0, 30.0, 80.0, 20.0),
            block("左2", 10.0, 130.0, 80.0, 20.0),
            block("右2", 200.0, 130.0, 80.0, 20.0),
            block("左1", 10.0, 30.0, 80.0, 20.0),
        ];
        assert_eq!(
            to_plain_text(&blocks),
            "左1\n左2\n右1\n右2",
            "columns are read top-to-bottom, then left-to-right"
        );
    }

    #[test]
    fn narrow_x_gaps_keep_one_column_top_to_bottom() {
        // 同一栏内词间只有窄空白：仍按原来的 y 分带、同带从左到右。
        let blocks = vec![
            block("b", 60.0, 10.0, 40.0, 20.0),
            block("a", 10.0, 10.0, 40.0, 20.0),
            block("c", 10.0, 80.0, 40.0, 20.0),
        ];
        assert_eq!(
            to_plain_text(&blocks),
            "a b\nc",
            "a narrow x gap does not split a single column"
        );
    }

    #[test]
    fn rows_further_apart_than_the_threshold_become_separate_lines() {
        let blocks = vec![
            block("second", 10.0, 100.0, 80.0, 20.0),
            block("first", 10.0, 10.0, 80.0, 20.0),
        ];
        assert_eq!(to_plain_text(&blocks), "first\nsecond");
    }

    #[test]
    fn a_slightly_ragged_row_stays_one_line() {
        // 20px tall boxes -> threshold 12; each step is under it.
        let blocks = vec![
            block("a", 10.0, 10.0, 20.0, 20.0),
            block("b", 40.0, 14.0, 20.0, 20.0),
            block("c", 70.0, 18.0, 20.0, 20.0),
        ];
        assert_eq!(to_plain_text(&blocks), "a b c");
    }

    #[test]
    fn blank_text_is_dropped_but_keeps_its_line() {
        let blocks = vec![
            block("  ", 10.0, 10.0, 20.0, 20.0),
            block("kept", 40.0, 10.0, 20.0, 20.0),
        ];
        assert_eq!(to_plain_text(&blocks), "kept");
    }

    #[test]
    fn degenerate_polygons_do_not_panic() {
        let blocks = vec![
            Block {
                text: "no poly".into(),
                poly: Vec::new(),
            },
            block("real", 10.0, 200.0, 20.0, 20.0),
        ];
        assert_eq!(to_plain_text(&blocks), "no poly\nreal");
    }

    #[test]
    fn offset_title_and_body_stay_one_column_in_y_order() {
        // 右对齐标题两行压在左对齐窄正文上方：x 间断 250px，但 y 跨度完全错位。
        let blocks = vec![
            block("正文1", 100.0, 100.0, 100.0, 20.0),
            block("标题2", 450.0, 40.0, 50.0, 20.0),
            block("正文2", 100.0, 130.0, 100.0, 20.0),
            block("标题1", 450.0, 10.0, 50.0, 20.0),
        ];
        assert_eq!(
            to_plain_text(&blocks),
            "标题1\n标题2\n正文1\n正文2",
            "a vertically offset title must not be cut off as a separate column"
        );
    }

    #[test]
    fn overlapping_y_bands_still_split_into_columns() {
        // 左栏四行、右栏两行，但两侧 y 区间重叠：空白贯穿双方跨度，仍应分栏。
        let blocks = vec![
            block("右1", 300.0, 50.0, 80.0, 20.0),
            block("左4", 10.0, 140.0, 80.0, 20.0),
            block("右2", 300.0, 100.0, 80.0, 20.0),
            block("左1", 10.0, 20.0, 80.0, 20.0),
            block("左3", 10.0, 90.0, 80.0, 20.0),
            block("左2", 10.0, 55.0, 80.0, 20.0),
        ];
        assert_eq!(
            to_plain_text(&blocks),
            "左1\n左2\n左3\n左4\n右1\n右2",
            "side-by-side columns with overlapping y bands are still split"
        );
    }
}

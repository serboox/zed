pub fn rectangle(class: &str, left: usize, width: usize) -> String {
    format!("<rect class=\"{class}\" x=\"{left}\" y=\"0\" width=\"{width}\" height=\"80\"/>")
}

pub fn drawing(content: &str) -> String {
    format!("<svg width=\"400\" height=\"80\" style=\"display:block\">{content}</svg>")
}

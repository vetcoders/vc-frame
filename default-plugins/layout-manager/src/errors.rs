use zellij_tile::prelude::*;

/// Format a layout parsing error into a detailed error string
pub fn format_kdl_error(error: LayoutParsingError) -> String {
    match error {
        LayoutParsingError::KdlError {
            mut kdl_error,
            file_name,
            source_code,
        } => {
            use miette::{GraphicalReportHandler, NamedSource, Report};

            kdl_error.help_message =
                Some("https://zellij.dev/documentation/creating-a-layout.html".to_owned());
            let report: Report = kdl_error.into();
            let report = report.with_source_code(NamedSource::new(file_name, source_code));

            let handler = GraphicalReportHandler::new();
            let mut output = String::new();
            handler.render_report(&mut output, report.as_ref()).unwrap();
            output
        },
        LayoutParsingError::SyntaxError => {
            format!(
                "Failed to deserialize KDL node. \nPossible reasons:\n{}\n{}\n{}\n{}",
                "- Missing `;` after a node name, eg. { node; another_node; }",
                "- Missing quotations (\") around an argument node eg. { first_node \"argument_node\"; }",
                "- Missing an equal sign (=) between node arguments on a title line. eg. argument=\"value\"",
                "- Found an extraneous equal sign (=) between node child arguments and their values. eg. { argument=\"value\" }"
            )
        },
    }
}

use std::{
    collections::HashMap,
    fs::{copy, File},
    io::Write,
    path::{Path, PathBuf},
};

use crate::{
    app::{converter::heic_to_jpeg, progress::build_progress_bar_export, runtime::Config},
    exporters::exporter::{BalloonFormatter, Exporter, Writer},
};

use imessage_database::{
    error::{message::MessageError, plist::PlistParseError, table::TableError},
    message_types::{
        app::AppMessage,
        edited::EditedMessage,
        expressives::{BubbleEffect, Expressive, ScreenEffect},
        music::MusicMessage,
        url::URLMessage,
        variants::{BalloonProvider, CustomBalloon, Variant},
    },
    tables::{
        attachment::{Attachment, MediaType},
        messages::{BubbleType, Message},
        table::{Table, FITNESS_RECEIVER, ME, ORPHANED, UNKNOWN},
    },
    util::{
        dates::{format, readable_diff},
        dirs::home,
        plist::parse_plist,
    },
};

use uuid::Uuid;

const HEADER: &str = "<html>\n<meta charset=\"UTF-8\">";
const FOOTER: &str = "</html>";
const STYLE: &str = include_str!("resources/style.css");

pub struct HTML<'a> {
    /// Data that is setup from the application's runtime
    pub config: &'a Config<'a>,
    /// Handles to files we want to write messages to
    /// Map of internal unique chatroom ID to a filename
    pub files: HashMap<i32, PathBuf>,
    /// Path to file for orphaned messages
    pub orphaned: PathBuf,
}

impl<'a> Exporter<'a> for HTML<'a> {
    fn new(config: &'a Config) -> Self {
        let mut orphaned = config.export_path();
        orphaned.push(ORPHANED);
        orphaned.set_extension("html");
        HTML {
            config,
            files: HashMap::new(),
            orphaned,
        }
    }

    fn iter_messages(&mut self) -> Result<(), TableError> {
        // Tell the user what we are doing
        eprintln!(
            "Exporting to {} as html...",
            self.config.export_path().display()
        );

        // Write orphaned file headers
        HTML::write_headers(&self.orphaned);

        // Set up progress bar
        let mut current_message = 0;
        let total_messages = Message::get_count(&self.config.db);
        let pb = build_progress_bar_export(total_messages);

        let mut statement = Message::get(&self.config.db);

        let messages = statement
            .query_map([], |row| Ok(Message::from_row(row)))
            .unwrap();

        for message in messages {
            let mut msg = Message::extract(message)?;
            // Render the announcement in-line
            if msg.is_announcement() {
                let announcement = self.format_announcement(&msg);
                HTML::write_to_file(self.get_or_create_file(&msg), &announcement);
            }
            // Message replies and reactions are rendered in context, so no need to render them separately
            else if !msg.is_reaction() {
                msg.gen_text(&self.config.db);
                let message = self.format_message(&msg, 0)?;
                HTML::write_to_file(self.get_or_create_file(&msg), &message);
            }
            current_message += 1;
            pb.set_position(current_message);
        }
        pb.finish();

        eprintln!("Writing HTML footers...");
        self.files
            .iter()
            .for_each(|(_, path)| HTML::write_to_file(path, FOOTER));
        HTML::write_to_file(&self.orphaned, FOOTER);

        Ok(())
    }

    /// Create a file for the given chat, caching it so we don't need to build it later
    fn get_or_create_file(&mut self, message: &Message) -> &Path {
        match self.config.conversation(message.chat_id) {
            Some((chatroom, id)) => self.files.entry(*id).or_insert_with(|| {
                let mut path = self.config.export_path();
                path.push(self.config.filename(chatroom));
                path.set_extension("html");

                // If the file already exists , don't write the headers again
                // This can happen if multiple chats use the same group name
                if !path.exists() {
                    // Write headers if the file does not exist
                    HTML::write_headers(&path);
                }

                path
            }),
            None => &self.orphaned,
        }
    }
}

impl<'a> Writer<'a> for HTML<'a> {
    fn format_message(&self, message: &Message, indent_size: usize) -> Result<String, TableError> {
        // Data we want to write to a file
        let mut formatted_message = String::new();

        // Message div
        self.add_line(&mut formatted_message, "<div class=\"message\">", "", "");

        // Start message div
        if message.is_from_me {
            self.add_line(
                &mut formatted_message,
                &format!("<div class=\"sent {:?}\">", message.service()),
                "",
                "",
            );
        } else {
            self.add_line(&mut formatted_message, "<div class=\"received\">", "", "");
        }

        // Add message date
        self.add_line(
            &mut formatted_message,
            &self.get_time(message),
            "<p><span class=\"timestamp\">",
            "</span>",
        );

        // Add message sender
        self.add_line(
            &mut formatted_message,
            self.config.who(&message.handle_id, message.is_from_me),
            "<span class=\"sender\">",
            "</span></p>",
        );

        // Useful message metadata
        let message_parts = message.body();
        let mut attachments = Attachment::from_message(&self.config.db, message)?;
        let mut replies = message.get_replies(&self.config.db)?;
        let reactions = message.get_reactions(&self.config.db, &self.config.reactions)?;

        // Index of where we are in the attachment Vector
        let mut attachment_index: usize = 0;

        // Add message subject
        if let Some(subject) = &message.subject {
            // Add message sender
            self.add_line(
                &mut formatted_message,
                subject,
                "<p>Subject: <span class=\"subject\">",
                "</span></p>",
            );
        }

        // If message was removed, display it
        if message_parts.is_empty() && message.is_edited() {
            // If this works, we want to format it as an announcement, so we early return for the Ok()
            let edited = match self.format_edited(message, "") {
                Ok(s) => return Ok(s),
                Err(why) => format!("{}, {}", message.guid, why),
            };
            self.add_line(
                &mut formatted_message,
                &edited,
                "<div class=\"edited\">",
                "</div>",
            );
        }

        // Generate the message body from it's components
        for (idx, message_part) in message_parts.iter().enumerate() {
            // Write the part div start
            self.add_line(
                &mut formatted_message,
                "<hr><div class=\"message_part\">",
                "",
                "",
            );

            match message_part {
                BubbleType::Text(text) => {
                    // Render edited messages
                    if message.is_edited() {
                        let edited = match self.format_edited(message, "") {
                            Ok(s) => s,
                            Err(why) => format!("{}, {}", message.guid, why),
                        };
                        self.add_line(
                            &mut formatted_message,
                            &edited,
                            "<div class=\"edited\">",
                            "</div>",
                        );
                    } else if text.starts_with(FITNESS_RECEIVER) {
                        self.add_line(
                            &mut formatted_message,
                            &text.replace(FITNESS_RECEIVER, "You"),
                            "<span class=\"bubble\">",
                            "</span>",
                        );
                    } else {
                        self.add_line(
                            &mut formatted_message,
                            text,
                            "<span class=\"bubble\">",
                            "</span>",
                        );
                    }
                }
                BubbleType::Attachment => {
                    match attachments.get_mut(attachment_index) {
                        Some(attachment) => match self.format_attachment(attachment) {
                            Ok(result) => {
                                attachment_index += 1;
                                self.add_line(&mut formatted_message, &result, "", "");
                            }
                            Err(result) => {
                                self.add_line(
                                    &mut formatted_message,
                                    result,
                                    "<span class=\"attachment_error\">Unable to locate attachment: ",
                                    "</span>",
                                );
                            }
                        },
                        // Attachment does not exist in attachments table
                        None => self.add_line(
                            &mut formatted_message,
                            "Attachment does not exist!",
                            "",
                            "",
                        ),
                    }
                }
                BubbleType::App => match self.format_app(message, &mut attachments, "") {
                    Ok(ok_bubble) => self.add_line(
                        &mut formatted_message,
                        &ok_bubble,
                        "<div class=\"app\">",
                        "</div>",
                    ),
                    Err(why) => self.add_line(
                        &mut formatted_message,
                        &format!("Unable to format {:?} message: {why}", message.variant()),
                        "<div class=\"app_error\">",
                        "</div>",
                    ),
                },
            };

            // Write the part div end
            self.add_line(&mut formatted_message, "</div>", "", "");

            // Handle expressives
            if message.expressive_send_style_id.is_some() {
                self.add_line(
                    &mut formatted_message,
                    self.format_expressive(message),
                    "<span class=\"expressive\">",
                    "</span>",
                );
            }

            // Handle Reactions
            if let Some(reactions) = reactions.get(&idx) {
                self.add_line(
                    &mut formatted_message,
                    "<hr><p>Reactions:</p>",
                    "<div class=\"reactions\">",
                    "",
                );
                reactions
                    .iter()
                    .try_for_each(|reaction| -> Result<(), TableError> {
                        self.add_line(
                            &mut formatted_message,
                            &self.format_reaction(reaction)?,
                            "<div class=\"reaction\">",
                            "</div>",
                        );
                        Ok(())
                    })?;
                self.add_line(&mut formatted_message, "</div>", "", "")
            }

            // Handle Replies
            if let Some(replies) = replies.get_mut(&idx) {
                self.add_line(&mut formatted_message, "<div class=\"replies\">", "", "");
                replies
                    .iter_mut()
                    .try_for_each(|reply| -> Result<(), TableError> {
                        reply.gen_text(&self.config.db);
                        if !reply.is_reaction() {
                            // Set indent to 1 so we know this is a recursive call
                            self.add_line(
                                &mut formatted_message,
                                &self.format_message(reply, 1)?,
                                "<div class=\"reply\">",
                                "</div>",
                            );
                        }
                        Ok(())
                    })?;
                self.add_line(&mut formatted_message, "</div>", "", "")
            }
        }

        // Add a note if the message is a reply and not rendered in a thread
        if message.is_reply() && indent_size == 0 {
            self.add_line(
                &mut formatted_message,
                "This message responded to an earlier message.",
                "<span class=\"reply_context\">",
                "</span>",
            );
        }

        // End message type div
        self.add_line(&mut formatted_message, "</div>", "", "");

        // End message div
        self.add_line(&mut formatted_message, "</div>", "", "");

        Ok(formatted_message)
    }

    fn format_attachment(&self, attachment: &'a mut Attachment) -> Result<String, &'a str> {
        match attachment.path() {
            Some(path) => {
                if let Some(path_str) = path.as_os_str().to_str() {
                    // Resolve the attachment path if necessary
                    let resolved_attachment_path = if path.starts_with("~") {
                        path_str.replace('~', &home())
                    } else {
                        path_str.to_owned()
                    };

                    // Perform optional copy + convert
                    if !self.config.options.no_copy {
                        let qualified_attachment_path = Path::new(&resolved_attachment_path);
                        match attachment.extension() {
                            Some(ext) => {
                                // Create a path to copy the file to
                                let mut copy_path = self.config.attachment_path();
                                copy_path.push(Uuid::new_v4().to_string());
                                // If the image is a HEIC, convert it to PNG, otherwise perform the copy
                                if ext == "heic" || ext == "HEIC" {
                                    // Write the converted file
                                    copy_path.set_extension("jpg");
                                    match heic_to_jpeg(qualified_attachment_path, &copy_path) {
                                        Some(_) => {}
                                        None => {
                                            // It is kind of odd to use Ok() on the failure here, but the Err()
                                            // this function returns is used for when files are missing, not when
                                            // conversion fails. Perhaps this should be a Result<String, Enum>
                                            // of some kind, but this conversion failure is quite rare.
                                            return Ok(format!(
                                                "Unable to convert and display file: {}",
                                                &attachment.transfer_name
                                            ));
                                        }
                                    }
                                } else {
                                    // Just copy the file
                                    copy_path.set_extension(ext);
                                    if qualified_attachment_path.exists() {
                                        if let Err(why) = copy(qualified_attachment_path, &copy_path) {
                                            eprintln!("Unable to copy {qualified_attachment_path:?}: {why}")
                                        };
                                    } else {
                                        return Err(&attachment.transfer_name);
                                    }
                                }
                                // Update the attachment
                                attachment.copied_path =
                                    Some(copy_path.to_string_lossy().to_string());
                            }
                            None => {
                                return Err(&attachment.transfer_name);
                            }
                        }
                    }

                    let embed_path = match &attachment.copied_path {
                        Some(path) => path,
                        None => &resolved_attachment_path,
                    };

                    Ok(match attachment.mime_type() {
                        MediaType::Image(_) => {
                            format!("<img src=\"{embed_path}\" loading=\"lazy\">")
                        }
                        MediaType::Video(media_type) => {
                            format!("<video controls> <source src=\"{embed_path}\" type=\"{media_type}\"> </video>")
                        }
                        MediaType::Audio(media_type) => {
                            format!("<audio controls src=\"{embed_path}\" type=\"{media_type}\" </audio>")
                        }
                        MediaType::Text(_) => {
                            format!(
                                "<a href=\"file://{embed_path}\">Click to download {}</a>",
                                attachment.transfer_name
                            )
                        }
                        MediaType::Application(_) => format!(
                            "<a href=\"file://{embed_path}\">Click to download {}</a>",
                            attachment.transfer_name
                        ),
                        MediaType::Unknown => {
                            format!("<p>Unknown attachment type: {embed_path}</p> <a href=\"file://{embed_path}\">Download</a>")
                        }
                        MediaType::Other(media_type) => {
                            format!("<p>Unable to embed {media_type} attachments: {embed_path}</p>")
                        }
                    })
                } else {
                    Err(&attachment.transfer_name)
                }
            }
            None => Err(&attachment.transfer_name),
        }
    }

    fn format_app(
        &self,
        message: &'a Message,
        attachments: &mut Vec<Attachment>,
        indent: &str,
    ) -> Result<String, PlistParseError> {
        if let Variant::App(balloon) = message.variant() {
            let mut app_bubble = String::new();

            match message.payload_data(&self.config.db) {
                Some(payload) => {
                    let parsed = parse_plist(&payload)?;

                    let res = if message.is_url() {
                        // Handle the URL case
                        match balloon {
                            CustomBalloon::URL => {
                                self.format_url(&URLMessage::from_map(&parsed)?, indent)
                            }
                            CustomBalloon::Music => {
                                self.format_music(&MusicMessage::from_map(&parsed)?, indent)
                            }
                            _ => unreachable!(),
                        }
                    } else {
                        match AppMessage::from_map(&parsed) {
                            Ok(bubble) => match balloon {
                                CustomBalloon::Application(bundle_id) => {
                                    self.format_generic_app(&bubble, bundle_id, attachments, indent)
                                }
                                CustomBalloon::Handwriting => {
                                    self.format_handwriting(&bubble, indent)
                                }
                                CustomBalloon::ApplePay => self.format_apple_pay(&bubble, indent),
                                CustomBalloon::Fitness => self.format_fitness(&bubble, indent),
                                CustomBalloon::Slideshow => self.format_slideshow(&bubble, indent),
                                _ => unreachable!(),
                            },
                            Err(why) => return Err(why),
                        }
                    };
                    app_bubble.push_str(&res);
                }
                None => {
                    // Sometimes, URL messages are missing their payloads
                    if message.is_url() {
                        if let Some(text) = &message.text {
                            let mut out_s = String::new();
                            out_s.push_str("<a href=\"");
                            out_s.push_str(text);
                            out_s.push_str("\">");

                            out_s.push_str("<div class=\"app_header\"><div class=\"name\">");
                            out_s.push_str(text);
                            out_s.push_str("</div></div>");

                            out_s.push_str("<div class=\"app_footer\"><div class=\"caption\">");
                            out_s.push_str(text);
                            out_s.push_str("</div></div></a>");

                            return Ok(out_s);
                        }
                    }
                    return Err(PlistParseError::NoPayload);
                }
            }
            Ok(app_bubble)
        } else {
            Err(PlistParseError::WrongMessageType)
        }
    }

    fn format_reaction(&self, msg: &Message) -> Result<String, TableError> {
        match msg.variant() {
            Variant::Reaction(_, added, reaction) => {
                if !added {
                    return Ok("".to_string());
                }
                Ok(format!(
                    "<span class=\"reaction\"><b>{:?}</b> by {}</span>",
                    reaction,
                    self.config.who(&msg.handle_id, msg.is_from_me),
                ))
            }
            Variant::Sticker(_) => {
                let mut paths = Attachment::from_message(&self.config.db, msg)?;
                let who = self.config.who(&msg.handle_id, msg.is_from_me);
                // Sticker messages have only one attachment, the sticker image
                Ok(match paths.get_mut(0) {
                    Some(sticker) => match self.format_attachment(sticker) {
                        Ok(img) => {
                            Some(format!("{img}<span class=\"reaction\"> from {who}</span>"))
                        }
                        Err(_) => None,
                    },
                    None => None,
                }
                .unwrap_or_else(|| {
                    format!("<span class=\"reaction\">Sticker from {who} not found!</span>")
                }))
            }
            _ => unreachable!(),
        }
    }

    fn format_expressive(&self, msg: &'a Message) -> &'a str {
        match msg.get_expressive() {
            Expressive::Screen(effect) => match effect {
                ScreenEffect::Confetti => "Sent with Confetti",
                ScreenEffect::Echo => "Sent with Echo",
                ScreenEffect::Fireworks => "Sent with Fireworks",
                ScreenEffect::Balloons => "Sent with Balloons",
                ScreenEffect::Heart => "Sent with Heart",
                ScreenEffect::Lasers => "Sent with Lasers",
                ScreenEffect::ShootingStar => "Sent with Shooting Start",
                ScreenEffect::Sparkles => "Sent with Sparkles",
                ScreenEffect::Spotlight => "Sent with Spotlight",
            },
            Expressive::Bubble(effect) => match effect {
                BubbleEffect::Slam => "Sent with Slam",
                BubbleEffect::Loud => "Sent with Loud",
                BubbleEffect::Gentle => "Sent with Gentle",
                BubbleEffect::InvisibleInk => "Sent with Invisible Ink",
            },
            Expressive::Unknown(effect) => effect,
            Expressive::None => "",
        }
    }

    fn format_announcement(&self, msg: &'a Message) -> String {
        let mut who = self.config.who(&msg.handle_id, msg.is_from_me);
        // Rename yourself so we render the proper grammar here
        if who == ME {
            who = "You"
        }
        let timestamp = format(&msg.date(&self.config.offset));
        format!(
            "\n<div class =\"announcement\"><p><span class=\"timestamp\">{timestamp}</span> {who} named the conversation <b>{}</b></p></div>\n",
            msg.group_title.as_deref().unwrap_or(UNKNOWN)
        )
    }
    fn format_edited(&self, msg: &'a Message, _: &str) -> Result<String, MessageError> {
        if let Some(payload) = msg.message_summary_info(&self.config.db) {
            let edited_message =
                EditedMessage::from_map(&payload).map_err(MessageError::PlistParseError)?;

            let mut out_s = String::new();
            let mut previous_timestamp: Option<&i64> = None;

            if edited_message.is_deleted() {
                let who = if msg.is_from_me { "You" } else { "They" };
                let timestamp = format(&msg.date(&self.config.offset));

                out_s.push_str(&format!(
                    "<div class =\"announcement\"><p><span class=\"timestamp\">{timestamp}</span> {who} deleted a message.</p></div>"
                ));
            } else {
                out_s.push_str("<table>");

                for i in 0..edited_message.items() {
                    let last = i == edited_message.items() - 1;

                    if let Some((timestamp, text, _)) = edited_message.item_at(i) {
                        match previous_timestamp {
                            None => out_s.push_str(&self.edited_to_html("", text, last)),
                            Some(prev_timestamp) => {
                                let end = msg.get_local_time(timestamp, &self.config.offset);
                                let start = msg.get_local_time(prev_timestamp, &self.config.offset);

                                let diff = readable_diff(start, end).unwrap_or_default();
                                out_s.push_str(&self.edited_to_html(
                                    &format!("Edited {diff} later"),
                                    text,
                                    last,
                                ))
                            }
                        }

                        // Update the previous timestamp for the next loop
                        previous_timestamp = Some(timestamp);
                    }
                }

                out_s.push_str("</table>");
            }

            return Ok(out_s);
        }
        Err(MessageError::PlistParseError(PlistParseError::NoPayload))
    }

    fn write_to_file(file: &Path, text: &str) {
        let mut file = File::options()
            .append(true)
            .create(true)
            .open(file)
            .unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }
}

impl<'a> BalloonFormatter for HTML<'a> {
    fn format_url(&self, balloon: &URLMessage, _: &str) -> String {
        let mut out_s = String::new();

        // Make the whole bubble clickable
        if let Some(url) = balloon.get_url() {
            out_s.push_str("<a href=\"");
            out_s.push_str(url);
            out_s.push_str("\">");
        }

        // Header section
        out_s.push_str("<div class=\"app_header\">");

        // Add preview images
        balloon.images.iter().for_each(|image| {
            out_s.push_str("<img src=\"");
            out_s.push_str(image);
            out_s.push_str("\" loading=\"lazy\", onerror=\"this.style.display='none'\">");
        });

        if let Some(site_name) = balloon.site_name {
            out_s.push_str("<div class=\"name\">");
            out_s.push_str(site_name);
            out_s.push_str("</div>");
        } else if let Some(url) = balloon.get_url() {
            out_s.push_str("<div class=\"name\">");
            out_s.push_str(url);
            out_s.push_str("</div>");
        }

        // Header end
        out_s.push_str("</div>");

        // Only write the footer if there is data to write
        if balloon.title.is_some() || balloon.summary.is_some() {
            out_s.push_str("<div class=\"app_footer\">");

            // Title
            if let Some(title) = balloon.title {
                out_s.push_str("<div class=\"caption\"><xmp>");
                out_s.push_str(title);
                out_s.push_str("</xmp></div>");
            }

            // Subtitle
            if let Some(summary) = balloon.summary {
                out_s.push_str("<div class=\"subcaption\"><xmp>");
                out_s.push_str(summary);
                out_s.push_str("</xmp></div>");
            }

            // End footer
            out_s.push_str("</div>");
        }

        // End the link
        if balloon.get_url().is_some() {
            out_s.push_str("</a>");
        }
        out_s
    }

    fn format_music(&self, balloon: &MusicMessage, _: &str) -> String {
        let mut out_s = String::new();

        // Header section
        out_s.push_str("<div class=\"app_header\">");

        // Name
        if let Some(track_name) = balloon.track_name {
            out_s.push_str("<div class=\"name\">");
            out_s.push_str(track_name);
            out_s.push_str("</div>");
        }

        // Add preview section
        if let Some(preview) = balloon.preview {
            out_s.push_str("<audio controls src=\"");
            out_s.push_str(preview);
            out_s.push_str("\" </audio>");
        }

        // Header end
        out_s.push_str("</div>");

        // Make the footer clickable so we can interact with the preview
        if let Some(url) = balloon.url {
            out_s.push_str("<a href=\"");
            out_s.push_str(url);
            out_s.push_str("\">");
        }

        // Only write the footer if there is data to write
        if balloon.artist.is_some() || balloon.album.is_some() {
            out_s.push_str("<div class=\"app_footer\">");

            // artist
            if let Some(artist) = balloon.artist {
                out_s.push_str("<div class=\"caption\"><xmp>");
                out_s.push_str(artist);
                out_s.push_str("</xmp></div>");
            }

            // Subtitle
            if let Some(album) = balloon.album {
                out_s.push_str("<div class=\"subcaption\"><xmp>");
                out_s.push_str(album);
                out_s.push_str("</xmp></div>");
            }

            // End footer
            out_s.push_str("</div>");
        }

        // End the link
        if balloon.url.is_some() {
            out_s.push_str("</a>");
        }
        out_s
    }

    fn format_handwriting(&self, _: &AppMessage, _: &str) -> String {
        String::from("Handwritten messages are not yet supported!")
    }

    fn format_apple_pay(&self, balloon: &AppMessage, _: &str) -> String {
        self.balloon_to_html(balloon, "Apple Pay", &mut [])
    }

    fn format_fitness(&self, balloon: &AppMessage, _: &str) -> String {
        self.balloon_to_html(balloon, "Fitness", &mut [])
    }

    fn format_slideshow(&self, balloon: &AppMessage, _: &str) -> String {
        self.balloon_to_html(balloon, "Slideshow", &mut [])
    }

    fn format_generic_app(
        &self,
        balloon: &AppMessage,
        bundle_id: &str,
        attachments: &mut Vec<Attachment>,
        _: &str,
    ) -> String {
        self.balloon_to_html(balloon, bundle_id, attachments)
    }
}

impl<'a> HTML<'a> {
    fn get_time(&self, message: &Message) -> String {
        let mut date = format(&message.date(&self.config.offset));
        let read_after = message.time_until_read(&self.config.offset);
        if let Some(time) = read_after {
            if !time.is_empty() {
                let who = if message.is_from_me { "them" } else { "you" };
                date.push_str(&format!(" (Read by {who} after {time})"));
            }
        }
        date
    }

    fn add_line(&self, string: &mut String, part: &str, pre: &str, post: &str) {
        if !part.is_empty() {
            string.push_str(pre);
            string.push_str(part);
            string.push_str(post);
            string.push('\n');
        }
    }

    fn write_headers(path: &Path) {
        // Write file header
        HTML::write_to_file(path, HEADER);

        // Write CSS
        HTML::write_to_file(path, "<style>\n");
        HTML::write_to_file(path, STYLE);
        HTML::write_to_file(path, "\n</style>");
    }

    fn edited_to_html(&self, timestamp: &str, text: &str, last: bool) -> String {
        let tag = match last {
            true => "tfoot",
            false => "tbody",
        };
        format!("<{tag}><tr><td><span class=\"timestamp\">{timestamp}</span></td><td>{text}</td></tr></{tag}>")
    }

    fn balloon_to_html(
        &self,
        balloon: &AppMessage,
        bundle_id: &str,
        attachments: &mut [Attachment],
    ) -> String {
        let mut out_s = String::new();
        if let Some(url) = balloon.url {
            out_s.push_str("<a href=\"");
            out_s.push_str(url);
            out_s.push_str("\">");
        }
        out_s.push_str("<div class=\"app_header\">");

        // Image
        if let Some(image) = balloon.image {
            out_s.push_str("<img src=\"");
            out_s.push_str(image);
            out_s.push_str("\">");
        } else if let Some(attachment) = attachments.get_mut(0) {
            out_s.push_str(&self.format_attachment(attachment).unwrap_or_default());
        }

        // Name
        out_s.push_str("<div class=\"name\">");
        out_s.push_str(balloon.app_name.unwrap_or(bundle_id));
        out_s.push_str("</div>");

        // Title
        if let Some(title) = balloon.title {
            out_s.push_str("<div class=\"image_title\">");
            out_s.push_str(title);
            out_s.push_str("</div>");
        }

        // Subtitle
        if let Some(subtitle) = balloon.subtitle {
            out_s.push_str("<div class=\"image_subtitle\">");
            out_s.push_str(subtitle);
            out_s.push_str("</div>");
        }

        // ldtext
        if let Some(ldtext) = balloon.ldtext {
            out_s.push_str("<div class=\"ldtext\">");
            out_s.push_str(ldtext);
            out_s.push_str("</div>");
        }

        // Header end, footer begin
        out_s.push_str("</div>");

        // Only write the footer if there is data to write
        if balloon.caption.is_some()
            || balloon.subcaption.is_some()
            || balloon.trailing_caption.is_some()
            || balloon.trailing_subcaption.is_some()
        {
            out_s.push_str("<div class=\"app_footer\">");

            // Caption
            if let Some(caption) = balloon.caption {
                out_s.push_str("<div class=\"caption\">");
                out_s.push_str(caption);
                out_s.push_str("</div>");
            }

            // Subcaption
            if let Some(subcaption) = balloon.subcaption {
                out_s.push_str("<div class=\"subcaption\">");
                out_s.push_str(subcaption);
                out_s.push_str("</div>");
            }

            // Trailing Caption
            if let Some(trailing_caption) = balloon.trailing_caption {
                out_s.push_str("<div class=\"trailing_caption\">");
                out_s.push_str(trailing_caption);
                out_s.push_str("</div>");
            }

            // Trailing Subcaption
            if let Some(trailing_subcaption) = balloon.trailing_subcaption {
                out_s.push_str("<div class=\"trailing_subcaption\">");
                out_s.push_str(trailing_subcaption);
                out_s.push_str("</div>");
            }

            out_s.push_str("</div>");
        }
        if balloon.url.is_some() {
            out_s.push_str("</a>");
        }
        out_s
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, Exporter, Options, HTML};
    use imessage_database::{tables::messages::Message, util::dirs::default_db_path};

    fn blank() -> Message {
        Message {
            rowid: i32::default(),
            guid: String::default(),
            text: None,
            service: Some("iMessage".to_string()),
            handle_id: i32::default(),
            subject: None,
            date: i64::default(),
            date_read: i64::default(),
            date_delivered: i64::default(),
            is_from_me: false,
            is_read: false,
            group_title: None,
            associated_message_guid: None,
            associated_message_type: Some(i32::default()),
            balloon_bundle_id: None,
            expressive_send_style_id: None,
            thread_originator_guid: None,
            thread_originator_part: None,
            date_edited: 0,
            chat_id: None,
            num_attachments: 0,
            num_replies: 0,
        }
    }

    fn fake_options() -> Options<'static> {
        Options {
            db_path: default_db_path(),
            no_copy: true,
            diagnostic: false,
            export_type: Some("html"),
            export_path: None,
            valid: true,
        }
    }

    #[test]
    fn can_create() {
        let options = fake_options();
        let config = Config::new(options).unwrap();
        let exporter = HTML::new(&config);
        assert_eq!(exporter.files.len(), 0);
    }

    #[test]
    fn can_get_time_valid() {
        // Create exporter
        let options = fake_options();
        let config = Config::new(options).unwrap();
        let exporter = HTML::new(&config);

        // Create fake message
        let mut message = blank();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        // May 17, 2022  8:29:42 PM
        message.date_delivered = 674526582885055488;
        // May 17, 2022  9:30:31 PM
        message.date_read = 674530231992568192;

        assert_eq!(
            exporter.get_time(&message),
            "May 17, 2022  5:29:42 PM (Read by you after 1 hour, 49 seconds)"
        );
    }

    #[test]
    fn can_get_time_invalid() {
        // Create exporter
        let options = fake_options();
        let config = Config::new(options).unwrap();
        let exporter = HTML::new(&config);

        // Create fake message
        let mut message = blank();
        // May 17, 2022  9:30:31 PM
        message.date = 674530231992568192;
        // May 17, 2022  9:30:31 PM
        message.date_delivered = 674530231992568192;
        // Wed May 18 2022 02:36:24 GMT+0000
        message.date_read = 674526582885055488;
        assert_eq!(exporter.get_time(&message), "May 17, 2022  6:30:31 PM");
    }

    #[test]
    fn can_add_line_no_indent() {
        // Create exporter
        let options = fake_options();
        let config = Config::new(options).unwrap();
        let exporter = HTML::new(&config);

        // Create sample data
        let mut s = String::new();
        exporter.add_line(&mut s, "hello world", "", "");

        assert_eq!(s, "hello world\n".to_string());
    }

    #[test]
    fn can_add_line() {
        // Create exporter
        let options = fake_options();
        let config = Config::new(options).unwrap();
        let exporter = HTML::new(&config);

        // Create sample data
        let mut s = String::new();
        exporter.add_line(&mut s, "hello world", "  ", "");

        assert_eq!(s, "  hello world\n".to_string());
    }

    #[test]
    fn can_add_line_pre_post() {
        // Create exporter
        let options = fake_options();
        let config = Config::new(options).unwrap();
        let exporter = HTML::new(&config);

        // Create sample data
        let mut s = String::new();
        exporter.add_line(&mut s, "hello world", "<div>", "</div>");

        assert_eq!(s, "<div>hello world</div>\n".to_string());
    }
}

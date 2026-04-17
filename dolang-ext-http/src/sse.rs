use std::{
    hash::{Hash, Hasher},
    mem, result, str,
};

use dolang::runtime::{
    Error, Instance, Object, Output, Result, Slot, State, Strand, object::Ref, object::TypeBuilder,
    value::TypeObject,
};

use crate::{global::Global, http::ResultExt as _};

pub(crate) struct Event;

pub(crate) struct EventIter {
    pub(crate) parser: SseParser,
}

pub(crate) struct EventAnnex {
    pub(crate) event_type: String,
    pub(crate) data: String,
    pub(crate) id: Option<String>,
    pub(crate) retry: Option<i64>,
}

#[derive(Default)]
struct ParsedEvent {
    event_type: Option<String>,
    data_lines: Vec<String>,
    id: Option<String>,
    retry: Option<i64>,
}

impl ParsedEvent {
    fn is_empty(&self) -> bool {
        self.event_type.is_none()
            && self.data_lines.is_empty()
            && self.id.is_none()
            && self.retry.is_none()
    }

    fn dispatch(self) -> Option<EventAnnex> {
        if self.data_lines.is_empty() {
            return None;
        }

        Some(EventAnnex {
            event_type: self
                .event_type
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "message".to_owned()),
            data: self.data_lines.join("\n"),
            id: self.id,
            retry: self.retry,
        })
    }
}

#[derive(Default)]
pub(crate) struct SseParser {
    buffer: Vec<u8>,
    current: ParsedEvent,
    pending: Option<EventAnnex>,
}

impl SseParser {
    pub(crate) fn push(&mut self, chunk: &[u8]) {
        self.buffer.extend_from_slice(chunk);
    }

    pub(crate) fn next_event(
        &mut self,
        eof: bool,
    ) -> result::Result<Option<EventAnnex>, str::Utf8Error> {
        if let Some(event) = self.pending.take() {
            return Ok(Some(event));
        }

        while let Some(line_end) = find_line_end(&self.buffer) {
            let mut line = self.buffer[..line_end].to_vec();
            let consumed = line_end + line_ending_len(&self.buffer[line_end..]);
            self.buffer.drain(..consumed);

            if line.last() == Some(&b'\r') {
                line.pop();
            }

            self.process_line(str::from_utf8(&line)?);
            if let Some(event) = self.pending.take() {
                return Ok(Some(event));
            }
        }

        if eof && !self.buffer.is_empty() {
            let line = str::from_utf8(&self.buffer)?.to_owned();
            self.process_line(&line);
            self.buffer.clear();
        }

        if eof {
            return Ok(self.current_event());
        }

        Ok(None)
    }

    fn process_line(&mut self, line: &str) {
        if line.is_empty() {
            self.pending = self.current_event();
            return;
        }

        if line.starts_with(':') {
            return;
        }

        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };

        match field {
            "event" => self.current.event_type = Some(value.to_owned()),
            "data" => self.current.data_lines.push(value.to_owned()),
            "id" if !value.contains('\0') => {
                self.current.id = Some(value.to_owned());
            }
            "id" => {}
            "retry" if !value.is_empty() && value.as_bytes().iter().all(u8::is_ascii_digit) => {
                self.current.retry = value.parse().ok();
            }
            _ => {}
        }
    }

    fn current_event(&mut self) -> Option<EventAnnex> {
        if self.current.is_empty() {
            return None;
        }

        let current = mem::take(&mut self.current);
        current.dispatch()
    }
}

fn find_line_end(buf: &[u8]) -> Option<usize> {
    buf.iter().position(|byte| matches!(byte, b'\n' | b'\r'))
}

fn line_ending_len(buf: &[u8]) -> usize {
    match buf {
        [b'\r', b'\n', ..] => 2,
        [b'\r' | b'\n', ..] => 1,
        _ => 0,
    }
}

impl<'v> Object<'v> for Event {
    const NAME: &'v str = "Event";
    const MODULE: &'v str = "http";
    type Annex = EventAnnex;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("type", |this, strand, out| {
                Output::set(strand, out, this.annex().event_type.as_str());
                Ok(())
            })
            .get("data", |this, strand, out| {
                Output::set(strand, out, this.annex().data.as_str());
                Ok(())
            })
            .get("id", |this, strand, out| {
                if let Some(id) = &this.annex().id {
                    Output::set(strand, out, id.as_str());
                }
                Ok(())
            })
            .get("retry", |this, strand, out| {
                if let Some(retry) = this.annex().retry {
                    Output::set(strand, out, retry);
                }
                Ok(())
            })
    }

    fn eq<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &dolang::runtime::Value<'v>,
    ) -> Result<'v, 's, bool> {
        let global = strand.state::<Global<'v>>();
        if let Some(other) = global.types.event.downcast(other) {
            let this = this.annex();
            let other = other.annex();
            Ok(this.event_type == other.event_type
                && this.data == other.data
                && this.id == other.id
                && this.retry == other.retry)
        } else {
            Err(Error::not_supported(strand))
        }
    }

    fn hash<'a, 's>(
        this: Instance<'v, 'a, Self>,
        _strand: &'a mut Strand<'v, 's>,
        hasher: &mut impl Hasher,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        annex.event_type.hash(hasher);
        annex.data.hash(hasher);
        annex.id.hash(hasher);
        annex.retry.hash(hasher);
        Ok(())
    }
}

impl<'v> Object<'v> for EventIter {
    const NAME: &'v str = "ResponseEventIter";
    const MODULE: &'v str = "http";
    const SLOTS: usize = 1;
    type Annex = State<'v, Global<'v>>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.supertype(TypeObject::Iter)
    }

    async fn input<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, this);
        Ok(())
    }

    async fn next<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let global = *this.annex();

        loop {
            let next = {
                let mut borrow = this.borrow_mut(strand)?;
                match borrow.parser.next_event(false) {
                    Ok(next) => next,
                    Err(_) => return Err(Error::runtime(strand, "invalid UTF-8")),
                }
            };

            if let Some(event) = next {
                global
                    .types
                    .event
                    .create_with_annex(strand, Event, event, out);
                return Ok(true);
            }

            let chunk = strand
                .with_slots(async move |strand, [mut response]| {
                    let borrow = this.borrow(strand)?;
                    Output::set(strand, &mut response, Ref::slot::<0>(&borrow));
                    drop(borrow);
                    let response =
                        global.types.response.downcast(&response).ok_or_else(|| {
                            Error::state_error(strand, "invalid response reference")
                        })?;

                    let mut response_borrow = response.borrow_mut(strand)?;
                    let inner = response_borrow
                        .inner
                        .as_mut()
                        .ok_or_else(|| Error::state_error(strand, "closed"))?;

                    inner.chunk().await.into_http(strand)
                })
                .await?;

            match chunk {
                Some(chunk) => {
                    this.borrow_mut(strand)?.parser.push(&chunk);
                }
                None => {
                    let next = {
                        let mut borrow = this.borrow_mut(strand)?;
                        match borrow.parser.next_event(true) {
                            Ok(next) => next,
                            Err(_) => return Err(Error::runtime(strand, "invalid UTF-8")),
                        }
                    };

                    if let Some(event) = next {
                        global
                            .types
                            .event
                            .create_with_annex(strand, Event, event, out);
                        return Ok(true);
                    }

                    return Ok(false);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SseParser;

    #[test]
    fn sse_parser_handles_chunk_boundaries_and_eof_dispatch() {
        let mut parser = SseParser::default();

        parser.push(b"event: tok");
        assert!(parser.next_event(false).unwrap().is_none());

        parser.push(b"en\nid: abc\nretry: 250\ndata: hel");
        assert!(parser.next_event(false).unwrap().is_none());

        parser.push(b"lo\r\ndata: world\r\n\r\ndata: tail");
        let first = parser.next_event(false).unwrap().unwrap();
        assert_eq!(first.event_type, "token");
        assert_eq!(first.id.as_deref(), Some("abc"));
        assert_eq!(first.retry, Some(250));
        assert_eq!(first.data, "hello\nworld");

        let second = parser.next_event(true).unwrap().unwrap();
        assert_eq!(second.event_type, "message");
        assert_eq!(second.id, None);
        assert_eq!(second.retry, None);
        assert_eq!(second.data, "tail");

        assert!(parser.next_event(true).unwrap().is_none());
    }

    #[test]
    fn sse_parser_rejects_invalid_utf8() {
        let mut parser = SseParser::default();
        parser.push(&[b'd', b'a', b't', b'a', b':', b' ', 0xff, b'\n']);
        assert!(parser.next_event(true).is_err());
    }
}

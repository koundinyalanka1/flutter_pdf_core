use crate::object::Dictionary;

#[derive(Debug, Clone, PartialEq)]
pub struct PdfStream {
    pub dictionary: Dictionary,
    pub data: Vec<u8>,
}

impl PdfStream {
    pub fn new(dictionary: Dictionary, data: Vec<u8>) -> Self {
        Self { dictionary, data }
    }
}

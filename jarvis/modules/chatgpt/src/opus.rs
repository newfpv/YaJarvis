//! Owned libopus states. Each state is used by one task and never shared concurrently.
use std::ptr::NonNull;
use libopus_sys as opus;
pub struct Encoder(NonNull<opus::OpusEncoder>);
pub struct Decoder(NonNull<opus::OpusDecoder>);
pub const SPEECH_BITRATE: i32 = 64_000;
// Opus allows moving an owned state between threads; concurrent access is not permitted.
unsafe impl Send for Encoder {}
unsafe impl Send for Decoder {}
impl Encoder {
    #[cfg(test)]
    pub fn new()->Result<Self,String>{
        Self::with_bitrate(SPEECH_BITRATE)
    }

    pub fn with_bitrate(bitrate: i32)->Result<Self,String>{
        let mut error=0;let pointer=unsafe{opus::opus_encoder_create(16000,1,opus::OPUS_APPLICATION_VOIP as i32,&mut error)};
        let encoder=NonNull::new(pointer).ok_or("Не создать кодек Opus")?;
        let result=Self(encoder);if error!=0{return Err(format!("Ошибка Opus {error}"));}
        let settings = [
            (opus::OPUS_SET_BITRATE_REQUEST, bitrate),
            (opus::OPUS_SET_COMPLEXITY_REQUEST, 10),
            (opus::OPUS_SET_SIGNAL_REQUEST, opus::OPUS_SIGNAL_VOICE as i32),
            (opus::OPUS_SET_MAX_BANDWIDTH_REQUEST, opus::OPUS_BANDWIDTH_WIDEBAND as i32),
            (opus::OPUS_SET_INBAND_FEC_REQUEST, 1),
            (opus::OPUS_SET_PACKET_LOSS_PERC_REQUEST, 10),
        ];
        for (request, value) in settings {
            let status = unsafe { opus::opus_encoder_ctl(pointer, request as i32, value) };
            if status != 0 { return Err(format!("Ошибка настройки Opus {request}: {status}")); }
        }
        Ok(result)
    }
    pub fn encode(&mut self,pcm:&[i16])->Result<Vec<u8>,String>{
        if pcm.len()!=320{return Err("Opus требует кадр 20 мс / 16 кГц".into());}
        let mut bytes=vec![0;1500];let n=unsafe{opus::opus_encode(self.0.as_ptr(),pcm.as_ptr(),320,bytes.as_mut_ptr(),bytes.len() as i32)};
        if n<0{return Err(format!("Ошибка кодирования Opus {n}"));}bytes.truncate(n as usize);Ok(bytes)
    }
}
impl Decoder {
    pub fn new()->Result<Self,String>{let mut error=0;let pointer=unsafe{opus::opus_decoder_create(48000,1,&mut error)};let pointer=NonNull::new(pointer).ok_or("Не создать декодер Opus")?;let result=Self(pointer);if error!=0{return Err(format!("Ошибка Opus {error}"));}Ok(result)}
    pub fn decode(&mut self,packet:Option<&[u8]>,fec:bool,frame_size:usize)->Result<Vec<i16>,String>{
        let packet=packet.unwrap_or(&[]);if packet.len()>65535{return Err("Слишком большой пакет Opus".into());}
        let mut pcm=vec![0;frame_size.clamp(120,5760)];let pointer=if packet.is_empty(){std::ptr::null()}else{packet.as_ptr()};
        let n=unsafe{opus::opus_decode(self.0.as_ptr(),pointer,packet.len() as i32,pcm.as_mut_ptr(),pcm.len() as i32,i32::from(fec))};
        if n<0{return Err(format!("Ошибка декодирования Opus {n}"));}pcm.truncate(n as usize);Ok(pcm)
    }
}
impl Drop for Encoder{fn drop(&mut self){unsafe{opus::opus_encoder_destroy(self.0.as_ptr());}}}
impl Drop for Decoder{fn drop(&mut self){unsafe{opus::opus_decoder_destroy(self.0.as_ptr());}}}
#[cfg(test)]mod tests{use super::*;#[test]fn codec_roundtrip_and_packet_loss(){let mut encoder=Encoder::new().unwrap();let mut decoder=Decoder::new().unwrap();let pcm:Vec<_>=(0..320).map(|i|((i as f64*0.12).sin()*5000.0) as i16).collect();let packet=encoder.encode(&pcm).unwrap();assert!(!packet.is_empty());assert_eq!(decoder.decode(Some(&packet),false,5760).unwrap().len(),960);assert_eq!(decoder.decode(None,false,960).unwrap().len(),960);assert!(encoder.encode(&[0;319]).is_err());}}

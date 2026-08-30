use cipher::{Block, BlockEncrypt, KeyInit};
use des::Des;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use zeroize::{Zeroize, Zeroizing};

use crate::ssh::ProxyTicket;

use super::{RfbError, RfbErrorKind, RfbPhase, RfbReader};

const VNC_AUTH_SECURITY_TYPE: u8 = 2;
const BANNER_LENGTH: usize = 12;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RfbVersion {
    V3_3,
    V3_7,
    V3_8,
}

impl RfbVersion {
    fn reply(self) -> &'static [u8; BANNER_LENGTH] {
        match self {
            Self::V3_3 => b"RFB 003.003\n",
            Self::V3_7 => b"RFB 003.007\n",
            Self::V3_8 => b"RFB 003.008\n",
        }
    }
}

fn parse_three_digits(bytes: &[u8]) -> Option<u16> {
    if bytes.len() != 3 || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(
        u16::from(bytes[0] - b'0') * 100
            + u16::from(bytes[1] - b'0') * 10
            + u16::from(bytes[2] - b'0'),
    )
}

fn parse_banner(banner: &[u8; BANNER_LENGTH]) -> Result<RfbVersion, RfbError> {
    if &banner[..4] != b"RFB " || banner[7] != b'.' || banner[11] != b'\n' {
        return Err(RfbError::new(
            RfbPhase::Banner,
            RfbErrorKind::ProtocolBanner,
            "version banner",
        ));
    }
    let major = parse_three_digits(&banner[4..7]).ok_or_else(|| {
        RfbError::new(
            RfbPhase::Banner,
            RfbErrorKind::ProtocolBanner,
            "version banner",
        )
    })?;
    let minor = parse_three_digits(&banner[8..11]).ok_or_else(|| {
        RfbError::new(
            RfbPhase::Banner,
            RfbErrorKind::ProtocolBanner,
            "version banner",
        )
    })?;

    match (major, minor) {
        (3, 3) => Ok(RfbVersion::V3_3),
        (3, 7) => Ok(RfbVersion::V3_7),
        (3, 8..) | (4.., _) => Ok(RfbVersion::V3_8),
        _ => Err(RfbError::new(
            RfbPhase::Banner,
            RfbErrorKind::ProtocolBanner,
            "unsupported version",
        )),
    }
}

pub async fn negotiate_version<S>(reader: &mut RfbReader<S>) -> Result<RfbVersion, RfbError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut banner = [0_u8; BANNER_LENGTH];
    reader
        .read_exact(&mut banner)
        .await
        .map_err(|source| RfbError::io(RfbPhase::Banner, source))?;
    let version = parse_banner(&banner)?;
    reader
        .write_all(version.reply())
        .await
        .map_err(|source| RfbError::io(RfbPhase::Banner, source))?;
    Ok(version)
}

async fn read_bounded_reason<S>(reader: &mut RfbReader<S>, phase: RfbPhase) -> Result<(), RfbError>
where
    S: AsyncRead + Unpin,
{
    let declared = reader
        .read_u32()
        .await
        .map_err(|source| RfbError::io(phase, source))?;
    let reason = reader
        .read_bounded_bytes(
            u64::from(declared),
            u64::from(reader.limits().max_text_bytes),
            "failure reason",
            phase,
        )
        .await?;
    drop(reason);
    Ok(())
}

pub async fn negotiate_security_type<S>(
    reader: &mut RfbReader<S>,
    version: RfbVersion,
) -> Result<(), RfbError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    reader
        .limits()
        .validate_for_phase(RfbPhase::SecurityTypes)?;
    match version {
        RfbVersion::V3_3 => {
            let security_type = reader
                .read_u32()
                .await
                .map_err(|source| RfbError::io(RfbPhase::SecurityTypes, source))?;
            if security_type == 0 {
                read_bounded_reason(reader, RfbPhase::SecurityTypes).await?;
                return Err(RfbError::new(
                    RfbPhase::SecurityTypes,
                    RfbErrorKind::SecurityFailure,
                    "server security rejection",
                ));
            }
            if security_type != u32::from(VNC_AUTH_SECURITY_TYPE) {
                return Err(RfbError::new(
                    RfbPhase::SecurityTypes,
                    RfbErrorKind::SecurityAllowlist,
                    "security type",
                ));
            }
        }
        RfbVersion::V3_7 | RfbVersion::V3_8 => {
            let count = reader
                .read_u8()
                .await
                .map_err(|source| RfbError::io(RfbPhase::SecurityTypes, source))?;
            if count == 0 {
                read_bounded_reason(reader, RfbPhase::SecurityTypes).await?;
                return Err(RfbError::new(
                    RfbPhase::SecurityTypes,
                    RfbErrorKind::SecurityFailure,
                    "server security rejection",
                ));
            }

            let mut offered = [0_u8; u8::MAX as usize];
            reader
                .read_exact(&mut offered[..usize::from(count)])
                .await
                .map_err(|source| RfbError::io(RfbPhase::SecurityTypes, source))?;
            if !offered[..usize::from(count)].contains(&VNC_AUTH_SECURITY_TYPE) {
                return Err(RfbError::new(
                    RfbPhase::SecurityTypes,
                    RfbErrorKind::SecurityAllowlist,
                    "security type",
                ));
            }
            reader
                .write_u8(VNC_AUTH_SECURITY_TYPE)
                .await
                .map_err(|source| RfbError::io(RfbPhase::SecurityTypes, source))?;
        }
    }
    Ok(())
}

async fn authenticate_vnc<S>(
    reader: &mut RfbReader<S>,
    ticket: &ProxyTicket,
) -> Result<(), RfbError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut challenge = Zeroizing::new([0_u8; 16]);
    reader
        .read_exact(&mut *challenge)
        .await
        .map_err(|source| RfbError::io(RfbPhase::Authentication, source))?;

    let mut key = Zeroizing::new([0_u8; 8]);
    for (destination, source) in key
        .iter_mut()
        .zip(ticket.expose_for_auth().as_bytes().iter().take(8))
    {
        *destination = source.reverse_bits();
    }

    let cipher = Des::new_from_slice(&*key).map_err(|_| {
        RfbError::new(
            RfbPhase::Authentication,
            RfbErrorKind::SecurityFailure,
            "authentication key",
        )
    })?;
    key.zeroize();

    let mut first = Block::<Des>::default();
    let mut second = Block::<Des>::default();
    first.copy_from_slice(&challenge[..8]);
    second.copy_from_slice(&challenge[8..]);
    cipher.encrypt_block(&mut first);
    cipher.encrypt_block(&mut second);

    let mut response = Zeroizing::new([0_u8; 16]);
    response[..8].copy_from_slice(&first);
    response[8..].copy_from_slice(&second);
    first.fill(0);
    second.fill(0);
    drop(cipher);
    challenge.zeroize();

    let write_result = reader.write_all(&*response).await;
    response.zeroize();
    write_result.map_err(|source| RfbError::io(RfbPhase::Authentication, source))
}

pub async fn read_security_result<S>(
    reader: &mut RfbReader<S>,
    version: RfbVersion,
) -> Result<(), RfbError>
where
    S: AsyncRead + Unpin,
{
    reader
        .limits()
        .validate_for_phase(RfbPhase::SecurityResult)?;
    let result = reader
        .read_u32()
        .await
        .map_err(|source| RfbError::io(RfbPhase::SecurityResult, source))?;
    if result == 0 {
        return Ok(());
    }
    if version == RfbVersion::V3_8 {
        read_bounded_reason(reader, RfbPhase::SecurityResult).await?;
    }
    Err(RfbError::new(
        RfbPhase::SecurityResult,
        RfbErrorKind::SecurityFailure,
        "authentication result",
    ))
}

pub(crate) async fn negotiate_security<S>(
    reader: &mut RfbReader<S>,
    ticket: ProxyTicket,
    version: RfbVersion,
) -> Result<(), RfbError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    negotiate_security_type(reader, version).await?;
    let authentication_result = authenticate_vnc(reader, &ticket).await;
    // A server may stall before SecurityResult; never retain the ticket across that wait.
    drop(ticket);
    authentication_result?;
    read_security_result(reader, version).await
}

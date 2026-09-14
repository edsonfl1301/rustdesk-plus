//! RustDesk OnlineRequest/OnlineResponse on the hbbs NAT-test port (21115).
//! Protocol: rustdesk/hbb_common/protos/rendezvous.proto and src/bytes_codec.rs.
use anyhow::{bail, ensure, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn varint(mut value: usize, out: &mut Vec<u8>) {
    while value >= 128 {
        out.push((value as u8 & 127) | 128);
        value >>= 7;
    }
    out.push(value as u8);
}

fn read_varint(data: &mut &[u8]) -> Result<usize> {
    let mut value = 0usize;
    for shift in (0..usize::BITS).step_by(7) {
        ensure!(!data.is_empty(), "truncated varint");
        let byte = data[0];
        *data = &data[1..];
        ensure!(
            (byte as usize & 127) <= (usize::MAX >> shift),
            "varint overflow"
        );
        value |= (byte as usize & 127) << shift;
        if byte & 128 == 0 {
            return Ok(value);
        }
    }
    bail!("varint overflow")
}

fn field<'a>(data: &mut &'a [u8], tag: usize) -> Result<&'a [u8]> {
    ensure!(
        read_varint(data)? == tag,
        "unexpected presence response field"
    );
    let size = read_varint(data)?;
    ensure!(size <= data.len(), "truncated presence response");
    let (value, rest) = data.split_at(size);
    *data = rest;
    Ok(value)
}

fn decode(mut data: &[u8], count: usize) -> Result<Vec<bool>> {
    let mut response = field(&mut data, (24 << 3) | 2)?;
    ensure!(data.is_empty(), "unexpected response trailing data");
    let states = field(&mut response, 10)?;
    ensure!(
        response.is_empty() && states.len() == (count + 7) / 8,
        "invalid presence bitmap"
    );
    Ok((0..count)
        .map(|i| states[i / 8] & (128 >> (i % 8)) != 0)
        .collect())
}

pub async fn query(address: &str, ids: &[String]) -> Result<Vec<bool>> {
    let mut peers = Vec::new();
    for id in ids {
        peers.push(18);
        varint(id.len(), &mut peers);
        peers.extend_from_slice(id.as_bytes());
    }
    let mut payload = vec![0xba, 1];
    varint(peers.len(), &mut payload);
    payload.extend(peers);
    ensure!(payload.len() <= 0x3fff, "presence request too large");
    let mut stream = tokio::net::TcpStream::connect(address).await?;
    let header = ((payload.len() << 2) | 1) as u16;
    stream.write_all(&header.to_le_bytes()).await?;
    stream.write_all(&payload).await?;
    let mut header = [0u8; 4];
    stream.read_exact(&mut header[..1]).await?;
    let width = (header[0] & 3) as usize + 1;
    stream.read_exact(&mut header[1..width]).await?;
    let size = (u32::from_le_bytes(header) >> 2) as usize;
    ensure!(size <= 4096, "presence response too large");
    let mut response = vec![0; size];
    stream.read_exact(&mut response).await?;
    decode(&response, ids.len())
}

pub async fn refresh(db: &sqlx::PgPool, address: &str) -> Result<()> {
    let ids = sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT rustdesk_id FROM devices WHERE deleted_at IS NULL AND rustdesk_id ~ '^[0-9]+$'",
    ).fetch_all(db).await?;
    for batch in ids.chunks(128) {
        let states = tokio::time::timeout(std::time::Duration::from_secs(5), query(address, batch))
            .await??;
        let online: Vec<String> = batch
            .iter()
            .zip(states)
            .filter_map(|(id, active)| active.then(|| id.clone()))
            .collect();
        // Refresh only confirmed online peers. Missing peers and failed queries expire
        // through the existing sweeper; fresh HTTP/agent presence remains valid.
        let recovered = sqlx::query_scalar::<_, i64>(
            "WITH refreshed AS (UPDATE devices SET online_since = CASE WHEN online = false THEN now() ELSE online_since END, online = true, last_seen_at = now() WHERE deleted_at IS NULL AND rustdesk_id = ANY($1) RETURNING id) SELECT count(*) FROM refreshed",
        ).bind(&online).fetch_one(db).await?;
        tracing::debug!(confirmed_online = recovered, "hbbs presence refreshed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_hbbs_bitmap_across_byte_boundary() {
        let result = decode(&[0xc2, 1, 4, 10, 2, 0xe5, 0xf0], 13).unwrap();
        assert_eq!(
            result,
            vec![true, true, true, false, false, true, false, true, true, true, true, true, false]
        );
    }

    #[test]
    fn rejects_invalid_or_truncated_response() {
        for bytes in [
            vec![],
            vec![0xc2, 1, 4, 10, 2, 0xe5],
            vec![0xc2, 1, 3, 10, 1, 255],
            vec![0xba, 1, 0],
        ] {
            assert!(decode(&bytes, 13).is_err());
        }
        assert!(decode(&[0xc2, 1, 3, 10, 1, 0], 1).unwrap() == vec![false]);
    }
}

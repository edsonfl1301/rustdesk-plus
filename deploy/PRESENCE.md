# Presença dos dispositivos

`HBBS_PRESENCE_ADDR=hbbs:21115` habilita a consulta de presença do servidor
RustDesk embutido. A API consulta os IDs numéricos já cadastrados a cada 30
segundos, antes de expirar os dispositivos sem contato há 60 segundos.

A consulta usa OnlineRequest/OnlineResponse, sem iniciar acesso remoto.
Somente IDs confirmados online têm `last_seen_at` atualizado. Respostas
inválidas, timeouts e dispositivos ausentes não produzem confirmação de
presença. Heartbeats HTTP e agentes continuam funcionando como antes.
Dispositivos excluídos não são restaurados pela consulta.

Sem a variável, mantém-se o comportamento anterior. O endereço deve apontar
para o hbbs que atende os dispositivos deste painel, pela rede interna Docker.

Fontes do protocolo:
- https://github.com/rustdesk/hbb_common/blob/main/protos/rendezvous.proto
- https://github.com/rustdesk/hbb_common/blob/main/src/bytes_codec.rs
- https://github.com/rustdesk/rustdesk-server/blob/master/src/rendezvous_server.rs

Diagnóstico de 08/09/2026: o painel recebia heartbeat de 3 dispositivos,
enquanto o hbbs confirmava 9 dos 12 dispositivos cadastrados. A ausência de
heartbeat HTTP não correspondia à indisponibilidade do acesso RustDesk.

# Guia do desenvolvedor — RustDesk Plus

Este guia descreve a implementação publicada em setembro de 2026 e os pontos de extensão. Leia também o [README](../README.md) para instalação e uso, e [PRESENCE.md](../deploy/PRESENCE.md) para o protocolo de presença. O código é a fonte de verdade caso um endpoint ou formato mude.

## Escopo e componentes

O RustDesk Plus combina um painel de inventário com o cliente e servidor RustDesk. O `plus-api` (Rust/Axum/SQLx) gerencia tenants, usuários, dispositivos, filiais, tags, instaladores e os endpoints compatíveis com o cliente desktop. O `dashboard` (Next.js) mostra e edita o inventário. O `hbbs` faz descoberta/sinalização; o `hbbr` faz relay. PostgreSQL persiste os dados do Plus. Caddy encaminha `/api/*`, `/admin/*`, `/t/*`, `/ws/*` e outras rotas de API ao `plus-api`; as demais vão ao dashboard. `agent` (Go) é uma função opcional de gerenciamento, diferente do serviço RustDesk que fornece acesso remoto.

O arquivo [docker-compose.plus.yml](../docker-compose.plus.yml) define os serviços e volumes. [compose.yaml](../compose.yaml) inclui esse arquivo para comandos Compose padrão. O [install.sh](../install.sh) usa `-f docker-compose.plus.yml` explicitamente. Novas instalações baixam imagens GHCR; `INSTALL_MODE=build` compila localmente. Um `.env` antigo sem as três variáveis de imagem continua no modo de build quando nenhum modo foi solicitado.

## Fluxo do cliente desktop

1. Configure no RustDesk o servidor de ID, chave pública e **API Server** para a URL do Plus. A conta usada no desktop é uma conta vinculada a um tenant; `super_admin` não tem tenant único e é recusado pelo login do cliente.
2. `POST /api/login` recebe `username` (e-mail) e `password` (senha da conta do painel). [client.rs](../plus-api/src/routes/client.rs) verifica o hash Argon2 no banco e emite JWT com usuário, papel e tenant. E-mail e senha que coincidirem com mais de um tenant produzem erro de ambiguidade.
3. O cliente chama `POST /api/currentUser`, `GET /api/ab` e as rotas de grupos. Todas as rotas de inventário exigem bearer token; as consultas SQL restringem os registros ao tenant do token e ignoram dispositivos excluídos.
4. `/api/ab` devolve `{ "data": "<JSON>" }`. A string interna contém `peers`, `tags` e `tag_colors`. Cada peer inclui `id`, `hostname`, `platform`, `alias`, `tags`, `note`, `same_server` e `password`. Os nomes das filiais entram como tags `Filial: ...`.
5. `GET /api/device-group/accessible`, `/api/users` e `/api/peers` fornecem a aba de grupos do cliente. `/api/peers` pagina os dispositivos e inclui estado online. Existem equivalentes sob `/t/:tenant_id/api/...` para compatibilidade com a URL usada pelos instaladores; o tenant autenticado continua sendo o do token nas rotas de catálogo.

As rotas `/api/ab/get` e `/api/ab` preservam compatibilidade com o catálogo legado. `ab_set` responde vazio: o painel e o banco Plus são a fonte do inventário, não as alterações locais feitas no cliente. Ao modificar o formato de uma resposta, confira o parser da versão do cliente RustDesk em uso e teste o cartão, o filtro por tag e o clique de conexão.

## Duas senhas e a conexão automática

A senha da conta do painel serve para obter o JWT e consultar o catálogo. A senha remota permanente pertence ao tenant e fica em `tenant_config` com a chave `rustdesk_password`; [config.rs](../plus-api/src/config.rs) a gera/carrega. O instalador Windows aplica essa senha ao RustDesk gerenciado. No painel, [devices/page.tsx](<../dashboard/src/app/(protected)/devices/page.tsx>) abre `rustdesk://<id>?password=<senha>`; assim o aplicativo recebe a senha sem perguntar ao operador.

No catálogo desktop, [client.rs](../plus-api/src/routes/client.rs) coloca a senha no campo `password` de cada peer apenas quando o papel do token é `admin` ou `operator`. Para `viewer`, o campo vem vazio. O RustDesk 1.4.9 interpreta esse campo em um peer do catálogo compartilhado como senha da conexão. A senha atravessa a API e pode permanecer no cache da estação; use HTTPS e proteja a conta e o computador da equipe de TI. Nunca inclua a senha em logs, exemplos, testes gravados ou no repositório.

**Limite de autorização:** isso controla a entrega automática da senha pelo Plus. O `hbbs`/`hbbr` e o dispositivo remoto não consultam o papel do painel para permitir uma sessão. Um `viewer` que saiba a senha remota, ou tenha outro método de autorização no RustDesk, ainda pode tentar conectar. Se o produto exigir bloqueio de sessão por papel, ele precisará de um mecanismo adicional no fluxo de acesso remoto, além de omitir o campo do catálogo. Revogar a conta também não invalida senhas já armazenadas em uma estação; para isso, altere a senha remota nos equipamentos e trate a rotação como migração operacional.

O papel e o tenant ficam no JWT emitido por [auth.rs](../plus-api/src/auth.rs), com validade atual de sete dias. Uma mudança de papel no banco não altera imediatamente um token já emitido. Para revogação imediata, adicione verificação de sessão/papel atual ou um mecanismo de invalidação de tokens; não dependa apenas da remoção do campo `password` após o próximo login.

## Cadastro e presença online

O instalador configura a API com tenant; o cliente/serviço envia heartbeat e informações do sistema. [client.rs](../plus-api/src/routes/client.rs) recebe `/api/heartbeat` e `/api/sysinfo`, com tenant no corpo, em `?tid=` ou no path `/t/:tenant_id/...`. Esses endpoints registram ou atualizam dispositivo, UUID, IP, hostname e sistema operacional. Os dispositivos podem herdar filial de outro dispositivo no mesmo IP e tenant. Revise esse mecanismo antes de usá-lo como prova forte de identidade: o tenant informado nesses endpoints não equivale ao login de um operador.

Heartbeat HTTP não mede sozinho a disponibilidade da conexão RustDesk. [presence.rs](../plus-api/src/presence.rs) consulta `OnlineRequest/OnlineResponse` no `hbbs:21115` com limite de cinco segundos. A rotina em [main.rs](../plus-api/src/main.rs) confirma IDs online a cada 30 segundos e marca offline registros sem confirmação por mais de 60 segundos. `/api/peers` faz consulta direta para mostrar o estado atual, com fallback para o estado salvo se a consulta falhar. Os bitmaps de resposta são validados; falhas não confirmam presença. A rotina ignora dispositivos excluídos.

## Servidor RustDesk e diagnóstico de conexões

O cliente RustDesk autenticado mais recente exigiu uma troca TCP segura que o `hbbs` OSS 1.1.15 usado anteriormente não fornecia. Isso causava `Failed to secure tcp: deadline has elapsed` antes da autenticação remota. [Dockerfile do servidor](../deploy/rustdesk-server/Dockerfile) compila somente `hbbs` do commit fixado `b9d886495f4cabcaba5e4ecea71262dcf2d42776`, derivado do [PR upstream #699](https://github.com/rustdesk/rustdesk-server/pull/699), e executa o teste `key_exchange` durante o build. `hbbr` permanece o binário oficial 1.1.15. Antes de atualizar esse pin, valide a troca segura, conexões diretas e via relay, e preserve o par de chaves do servidor.

Os erros têm causas diferentes:

| Sintoma | Primeiro ponto de verificação |
|---|---|
| `HTTP 404` na aba de grupos | Caddy encaminha `/api/*` e `/t/*`? As rotas `/api/device-group/accessible`, `/api/users` e `/api/peers` existem na API implantada? |
| `Failed to secure tcp` | Versão do `hbbs`, handshake seguro, acesso às portas de sinalização e logs de ambos os lados. |
| Solicitação de senha no desktop | Papel da conta, resposta de `/api/ab`, atualização do catálogo e senha instalada no peer. Não imprima o valor recebido. |
| Senha aceita, mas `No Displays` | O peer remoto informou lista vazia de displays. Verifique sessão/serviço e monitores do sistema remoto; o login e o relay já podem estar funcionando. |
| Conexão falha apenas na mesma rede/NAT | Compare sessão direta com relay e verifique `hbbr`/21117; alguns pares precisam de relay forçado. |

## Arquivos importantes para personalização

| Objetivo | Onde começar |
|---|---|
| Alterar login, catálogo, grupos ou papéis | `plus-api/src/routes/client.rs`, `plus-api/src/auth.rs` |
| Alterar senha por tenant e instalador | `plus-api/src/config.rs`, `plus-api/src/installer.rs`, `installer/main.go` |
| Alterar dispositivos, filiais e tags no painel | `dashboard/src/app/(protected)/devices/page.tsx`, `dashboard/src/lib/api.ts`, `plus-api/src/routes/admin.rs` |
| Alterar presença | `plus-api/src/presence.rs`, `plus-api/src/main.rs`, `deploy/PRESENCE.md` |
| Alterar o servidor de ID/relay | `deploy/rustdesk-server/Dockerfile`, `deploy/rustdesk-server/hbbs-entrypoint.sh` |
| Alterar portas, roteamento ou imagens | `docker-compose.plus.yml`, `deploy/Caddyfile`, `.env.example`, `install.sh` |
| Alterar dados persistidos | `plus-api/migrations/` e os modelos/queries SQLx correspondentes |
| Alterar publicação de imagens | `.github/workflows/publish-images.yml` |

O código do cliente RustDesk não é mantido por este repositório. Teste alterações do catálogo contra a versão concreta do cliente usada na equipe. Para branding do aplicativo, consulte `tools/rustdesk-builder/` e as funções de tenant branding; esse caminho é opcional e independente do login/catálogo.

## Desenvolvimento e validação

Use um tenant de teste, dispositivos de teste e URLs sem dados reais. Para uma alteração no backend, execute `cargo fmt --check` e `cargo test` no diretório `plus-api`; para o painel, execute os comandos de lint/build definidos em `dashboard/package.json`. Valide `docker compose -f docker-compose.plus.yml config --quiet` com um `.env` de teste. O workflow [publish-images.yml](../.github/workflows/publish-images.yml) constrói e publica `rustdesk-plus-api`, `rustdesk-plus-dashboard` e `rustdesk-plus-server` no GHCR em push para `main`; os Dockerfiles também são uma verificação de build. Tags `latest`, de Git e `sha-...` são geradas pela action.

Uma mudança no fluxo remoto precisa de teste real em uma estação: login da conta, catálogo e filiais, estado online, conexão a um peer de teste, senha automática para `admin`/`operator`, ausência da senha para `viewer`, acesso direto e relay. Compare o erro antes/depois e confira os logs do servidor sem registrar segredos. O teste que motivou esta versão distinguiu um timeout de handshake, um peer sem displays e uma conexão bem-sucedida com a senha remota correta; um resultado positivo em uma etapa não prova as outras.

## Implantação, backup e retorno

Antes de alterar uma instância existente, registre a imagem/commit em uso e faça backup verificável do compose, `.env`, chaves RustDesk, volumes de configuração e banco PostgreSQL. Um tar da pasta **não substitui** um dump consistente do PostgreSQL para migrações ou restauração de banco ativo. Guarde os backups fora do repositório público, valide checksum e faça um ensaio de restauração em ambiente separado.

Para mudanças somente na API, recrie apenas `plus-api` com o nome correto do projeto Compose. Verifique `GET /health`, inicialização e migrações, resposta do catálogo autenticado e uma conexão real. Não deduza o projeto a partir do nome de um diretório montado: use `-p rustdesk-plus` ou execute a partir da raiz correta, para evitar um stack paralelo. Guarde a imagem anterior para retorno rápido; se a migração alterou o esquema, planeje também o retorno do banco antes do deploy. Nunca publique `.env`, JWT, chaves privadas, senha remota ou dados dos dispositivos no GitHub.

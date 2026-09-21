# Cockpit Tools

Este projeto é baseado no projeto original [CockpitTools](https://github.com/jlcodes99/cockpit-tools).

Este repositório documenta apenas as adições locais e o uso. Ele não repete a lista completa de recursos do projeto original. O repositório contém apenas o código-fonte necessário para construção, não banco de dados local, dados de conta ou configuração de execução.

## Destaques locais

- Adicionado `workbuddy2api`
- Adicionado `qoder2api`

### `workbuddy2api`

Expõe o conjunto local de contas WorkBuddy como um serviço compatível com OpenAI. Suporta endpoints de Chat Completions e Responses, listagem de modelos, streaming, atualização de token, rotação de contas, tratamento por cooldown/breaker e limites de concorrência por conta. A API escuta na porta `7863`; os dados do painel administrativo ficam na porta `7864`.

### `qoder2api`

Expõe o estado de login do QoderWork como um gateway compatível com OpenAI Chat Completions. Ele atualiza credenciais locais, encaminha solicitações ao Qoder, mapeia nomes de modelos e agrega o SSE upstream quando o cliente solicita uma resposta sem streaming. O gateway escuta na porta `7866`.

### `cindy2api`

Adiciona o gateway da plataforma Cindy e gerenciamento de contas. O gateway pode descobrir credenciais locais do Cindy ou adicionar contas por telefone, código de e-mail ou OAuth.

- Fornece `/v1/models` e `/v1/chat/completions` compatíveis com OpenAI
- Suporta streaming, respostas sem streaming, pool de contas e verificação de saúde
- Suporta login por telefone no Cindy CN, e-mail internacional, OAuth e importação local
- Abre uma janela dedicada de verificação quando necessário
- Mantém as chaves do gateway isoladas das credenciais upstream
- Escuta na porta `7865`

### Automação e estabilidade do WorkBuddy

- Check-in, viagem e tarefas do centro de crescimento podem ser configurados separadamente
- O agendamento no backend inclui logs de execução e disparo manual
- O gateway nativo adiciona bloqueio de instância única, captura de porta, atualização de token, rotação de contas e limites de concorrência por conta que também alimentam os pesos de seleção

### Compatibilidade de protocolo e OAuth

- Conclui a tradução de Responses para Chat no gateway WorkBuddy e habilita o fluxo `spawn_agent` do Codex
- Usa o perfil confiável local do Chrome para fluxos de autorização OAuth

### Velocidade de build incremental

Move recursos do frontend, o binário da aplicação e o Tauri Context para um crate `context` dedicado, evitando que mudanças no frontend invalidem a biblioteca principal. As impressões digitais do build script são determinísticas, o Windows usa `rust-lld`, a verificação de tipos TypeScript é incremental, e `npm run build:app` é o ponto único de build de release.

## Uso

```bash
npm install
npm run tauri:dev
```

Use as páginas WorkBuddy e QoderWork para adicionar contas e abra o painel correspondente do gateway para copiar o Base URL e a API Key locais:

- WorkBuddy2API: `http://127.0.0.1:7863/v1`
- Qoder2API: `http://127.0.0.1:7866/v1`

Defina esses valores como Base URL e chave de API do cliente compatível com OpenAI. O serviço é destinado apenas a clientes locais ou de rede privada; não o exponha diretamente à internet pública.

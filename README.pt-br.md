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

## Uso

```bash
npm install
npm run tauri:dev
```

Use as páginas WorkBuddy e QoderWork para adicionar contas e abra o painel correspondente do gateway para copiar o Base URL e a API Key locais:

- WorkBuddy2API: `http://127.0.0.1:7863/v1`
- Qoder2API: `http://127.0.0.1:7866/v1`

Defina esses valores como Base URL e chave de API do cliente compatível com OpenAI. O serviço é destinado apenas a clientes locais ou de rede privada; não o exponha diretamente à internet pública.

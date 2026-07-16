# set --nowait (SetI fire-and-forget) 設計

日付: 2026-07-16
ステータス: 承認済み（実装は別セッション）

## 背景 / 動機

`listen` が 3610 を無期限に専有している間（例: 常駐アプリが `enl listen --count 1
--timeout-ms 0` で INF を待つ運用）、`set` / `get` は bind リトライ枯渇後に exit 5
となり、機器を操作できない。listen と操作系を同一ホストで共存させたい。

当初案は「送信系をエフェメラルポート化して従来どおり応答を待つ」だったが、
**実機検証で不成立と確定した**:

- LAN 直結ホストからエフェメラル送信元ポートで unicast Get を送ると全 7 機器が無応答。
- tcpdump で観測すると、機器は数 ms で応答を返しているが、宛先は**要求の送信元
  ポートではなく常に 3610 固定**（応答の送信元ポートはエフェメラル）。
- つまりエフェメラルで送って応答を待っても、応答は 3610 を握る listen に配達されて
  破棄され、one-shot 側は必ず timeout (exit 3) になる。CLAUDE.md の
  「最重要の落とし穴: ポート 3610 専有」が自宅全機器で実証された形。

なお SO_REUSEPORT による共存は、カーネルが受信データグラムをソケット間で振り分ける
ため応答が listen 側に吸われうること、std に API が無く依存追加（socket2 等）が
必要なことから不採用（CLAUDE.md 記載どおり）。

## 決定

応答を待たない送信専用モードを `set` に追加する。応答が要らなければ 3610 を
バインドする必要がなく、listen と無条件に共存できる。

- ESV を SetC (0x61, 応答要求) から **SetI (0x60, 応答不要)** に切り替える。
- ソケットは **エフェメラルポート** (`0.0.0.0:0`) にバインドし、送信のみ行う。
  受信はしない。
- 送信成功で即 exit 0。

実行確認は listen が受ける INF（状変通知）か後続の `get` に委ねる。one-shot の
「操作」と listen の「観測」を分離する UNIX 哲学的な整理であり、デーモン化や
ソケット共有より筋が良い。

## CLI

```
enl set <ip> <eoj> <epc> <edt> --nowait [--multicast]
```

- `--nowait`: SetI で送信し応答を待たない。listen が 3610 を専有していても使える。
- デフォルト（`--nowait` なし）は従来どおり 3610 バインド + SetC + 応答待ち。
  **既存動作・既存スキーマは一切変えない。**
- `--multicast` と併用可。SetI は応答が無いため multicast と相性が良い
  （従来 SetC + multicast は応答レポートが `ip` の機器のみという歪みがあった）。
  DEOJ が一致する LAN 上の全機器が実行する点は従来の multicast Set と同じ。
- `--nowait` 時は `--timeout-ms` は使われない（応答を待たないため）。ヘルプに明記。

## stdout JSON

```json
{
  "ip": "192.0.2.22",
  "eoj": "029101",
  "esv": "SetI",
  "result": "sent",
  "properties": [{ "epc": "80", "edt_hex": "30" }]
}
```

- 従来 set の `"result": "accepted"`（機器が受理を確認済み）に対し、
  `"result": "sent"`（送信のみ・実行未確認）で明確に区別する。
- `properties` は送信した要求プロパティのエコー（epc + edt_hex、辞書デコードが
  あれば従来同様併記）。
- `schema` コマンドの set 出力スキーマに `--nowait` 時の形を追記する。

## 制約（ドキュメント化必須）

- 機器リジェクト **SetI_SNA (0x50) は検知できない**（応答は 3610 宛てに返るため）。
  exit 0 は「送信できた」ことしか意味しない。
- 不正 EPC/EDT でも exit 0 になる。確実な受理確認が要る場面では従来の SetC
  （`--nowait` なし）を使う。

## exit code

| code | `--nowait` 時の意味 |
|---|---|
| 0 | 送信成功（実行は未確認） |
| 2 | CLI 引数エラー |
| 5 | ソケット作成・送信失敗 |
| 3 / 4 | 発生しない（応答を待たないため） |

## 実装

- `net.rs`: `open_ephemeral_socket()` を追加。`0.0.0.0:0` へバインド。
  AddrInUse は起き得ないためリトライ無し。失敗は従来同様 `ErrKind::Bind` (exit 5)。
- `commands.rs`: `set` に nowait 経路を追加。`request()`（3610 + 送受信）を通らず、
  SetI フレームをビルドしてエフェメラルソケットから `send_to` するのみ。
- `main.rs`: `Set` サブコマンドに `#[arg(long)] nowait: bool` を追加。
- `schema.rs`: set スキーマに nowait 時の出力を追記。
- CLAUDE.md「ポート 3610 専有」節と README: 実機検証結果（応答先は 3610 固定、
  エフェメラル + 応答待ちは不成立）と `--nowait` の位置づけを反映。
- バージョン 1.4.0 に bump（後方互換の機能追加）。

## テスト

- codec: SetI フレーム (ESV 0x60) のビルド/パースのラウンドトリップ
  （既存の Esv enum に SetI はあるため、カバレッジ確認して不足分のみ追加）。
- commands: nowait 経路が「正しい SetI フレームを送信し、受信せず即 return する」
  ことのユニットテスト（127.0.0.1 の受け側ソケットで送信バイト列を検証）。
- net: `open_ephemeral_socket()` が bind に成功し port != 3610 であること。
- 実機受け入れ検証（jarvis から）: listen が 3610 を専有した状態で
  `set --nowait` がリンクプラス照明を実際に操作でき、exit 0 になること。
  INF が listen 側に届くこと。

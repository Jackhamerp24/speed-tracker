# Speed Tracker

[English](README.md) · **Tiếng Việt**

Ứng dụng trên thanh menu cho biết model đứng sau coding agent của bạn đang phản hồi nhanh đến đâu:

- **TTFT**: thời gian từ lúc gửi yêu cầu đến token đầu tiên được sinh ra
- **Tốc độ**: số token đầu ra mỗi giây

Không cần cài đặt gì thêm. Mở app, dùng coding agent như bình thường, và số liệu tự hiện ra. App đọc những gì agent đã tự ghi xuống đĩa và quan sát hoạt động mạng của nó. App không bao giờ sửa cấu hình của agent, và không lưu lại prompt hay câu trả lời nào của bạn.

macOS là nền tảng chính. Bản Windows 11 có kèm theo và đang ở mức thử nghiệm (xem [Windows](#windows)).

| Live | Chọn harness để theo dõi | Dashboard |
| --- | --- | --- |
| ![Tab Live](docs/images/live.png) | ![Danh sách harness](docs/images/target-list.png) | ![Dashboard](docs/images/dashboard-trends.png) |

*Ảnh chụp dùng dữ liệu mẫu có sẵn trong app.*

## Các harness được hỗ trợ

"Harness" ở đây là coding agent bạn chạy: Claude Code, Codex, v.v. Khi khởi động, Speed Tracker tự xác định harness nào có trên máy và chỉ cho chọn những harness đó.

### Đọc từ dữ liệu phiên của chính harness

Số token và tên model là chính xác, vì lấy từ những gì harness tự ghi lại.

| Harness | Đọc từ đâu | Token | TTFT |
| --- | --- | --- | --- |
| **Claude Code** (terminal, và bản nằm trong app Claude desktop) | log phiên ở `~/.claude/projects` | chính xác | lúc bắt đầu khối thinking nếu câu trả lời suy nghĩ trước; nếu không thì byte phản hồi đầu tiên thấy trên mạng |
| **Codex** (CLI, và Codex trong app ChatGPT desktop) | log rollout ở `~/.codex/sessions` | chính xác | lúc bắt đầu mục reasoning hoặc message đầu tiên |
| **OMP** (oh-my-pi) và **Pi** | `~/.omp/agent/sessions`, `~/.pi/agent/sessions` | chính xác | số đo của chính harness |
| **OpenCode** | cơ sở dữ liệu SQLite, hoặc kho JSON đời cũ | chính xác | chỉ có với kho đời cũ; định dạng hiện tại không ghi thời điểm gửi yêu cầu nên TTFT để trống |
| **Gemini CLI** | log phiên ở `~/.gemini/tmp/…/chats` | chính xác | lúc thought đầu tiên đến; câu trả lời không có thought thì dùng số đo từ mạng |
| **Antigravity** (trình soạn thảo và `agy`) | cơ sở dữ liệu ở `~/.gemini/antigravity/conversations` | chính xác | số đo của chính Antigravity |
| **DeepSeek CLI** (`dsh`) | log ở `~/.dsh/sessions`, dạng thường hoặc nén Zstandard | chính xác | từ bước gửi yêu cầu đến sự kiện stream đầu tiên |

### Nhận diện qua tiến trình, đo thời gian từ hoạt động mạng

Các harness này không có dữ liệu phiên mà app đọc được. Thời gian lấy từ lưu lượng mạng của tiến trình, còn số token là **ước lượng** từ kích thước stream. Số ước lượng luôn có dấu `~`.

| Harness | macOS | Windows |
| --- | --- | --- |
| Qwen Code, Aider, Goose, Crush | có | cần bật bộ thu mạng tuỳ chọn |
| Amp, Droid, Kimi CLI, Cursor CLI, Cline | có | không |

### Các công cụ khác

Trỏ base URL của bất kỳ công cụ nào vào proxy cục bộ có sẵn thì stream của nó được đo chính xác. Đây là tính năng duy nhất cần cấu hình; xem [Proxy tuỳ chọn](#proxy-tuỳ-chọn).

### Mức độ đã kiểm thử

| | Tình trạng |
| --- | --- |
| Claude Code | đã theo dõi trực tiếp, đối chiếu với log phiên |
| Codex, OMP | đã đọc các phiên cũ từ dữ liệu thật; Codex cũng đã thấy trực tiếp một lần |
| Antigravity | đã đọc từ một hội thoại thật; mọi số đo thời gian khớp với mốc thời gian trong chính file đó |
| OpenCode, DeepSeek CLI | đã đọc từ dữ liệu thật |
| Gemini CLI | viết dựa trên mã nguồn của chính CLI và có test; chưa chạy với một câu trả lời thật |
| Harness chỉ nhận diện qua tiến trình | có test; chưa cái nào được chạy thật |

## Cài đặt

### macOS

Cần macOS 14 trở lên, chip Apple silicon hoặc Intel.

1. Tải `SpeedTracker-macOS.zip` từ [bản phát hành mới nhất](https://github.com/Jackhamerp24/speed-tracker/releases/latest).
2. Giải nén và kéo **Speed Tracker** vào `Applications`.
3. App chưa được ký bằng chứng chỉ nhà phát triển của Apple nên macOS chặn lần mở đầu tiên. Hãy nhấp chuột phải vào app rồi chọn **Open**, hoặc chạy:

   ```bash
   xattr -dr com.apple.quarantine "/Applications/Speed Tracker.app"
   ```

Một biểu tượng tia chớp sẽ hiện trên thanh menu. Lần đầu chạy, app đọc các phiên trong bảy ngày gần nhất nên có số liệu ngay.

### Windows

Cần Windows 11, x64 hoặc ARM64.

1. Tải `SpeedTracker-win-x64.zip` hoặc `SpeedTracker-win-arm64.zip` từ [bản phát hành mới nhất](https://github.com/Jackhamerp24/speed-tracker/releases/latest).
2. Giải nén toàn bộ và chạy `SpeedTracker.exe`. Không cần cài thêm gì.
3. App chưa được ký nên SmartScreen có thể cảnh báo: chọn **More info**, rồi **Run anyway**.

## Cách dùng

**Thanh menu.** Một con số duy nhất là tốc độ, được giữ ổn định. Nó bám theo stream khi chữ đang về. Khi model đang chờ, đang suy nghĩ hoặc đang chạy tool, nó giữ giá trị gần nhất chứ không tụt về 0. Khi cuộc gọi được ghi log xong, nó dừng ở con số chính xác. Tia chớp được tô đầy khi đang có cuộc gọi.

**Tab Live.** Cuộc gọi hiện tại: model, harness, TTFT, tốc độ, biểu đồ nhỏ và các cuộc gọi gần đây. Bấm **Watching** để chọn theo dõi cái gì:

- **Auto** bám theo harness nào đang hoạt động.
- **Một harness cụ thể** ghim tab Live và thanh menu vào harness đó.

Chỉ những harness có trên máy mới được liệt kê. Một harness được coi là có mặt khi lệnh hoặc app của nó đã được cài, khi nó có dữ liệu phiên, hoặc khi nó đang chạy. Danh sách được kiểm tra lúc khởi động và cập nhật trong khi app chạy.

**Tab Models.** Trung vị TTFT và tốc độ cho từng cặp harness và model.

**Dashboard.** Một cửa sổ xem lịch sử: tổng quan provider theo harness, xu hướng, phân bố và từng cuộc gọi. Ba bộ lọc harness, provider và model thu hẹp lẫn nhau, nên mỗi bộ lọc chỉ đưa ra những giá trị có cuộc gọi với hai bộ lọc còn lại. Chọn một model thì danh sách harness chỉ còn những harness từng dùng model đó.

**Settings.** Các harness tìm thấy, mở cùng lúc đăng nhập, và có hiện TTFT trên thanh menu hay không.

## Các con số được định nghĩa thế nào

| Con số | Định nghĩa |
| --- | --- |
| TTFT | Từ lúc gửi yêu cầu đến dấu hiệu sinh nội dung đầu tiên. Thinking được tính là đã sinh, nên model suy nghĩ trước không bị tính thời gian suy nghĩ vào độ trễ. |
| Tốc độ | Số token đầu ra chia cho thời gian từ token đầu tiên đến khi kết thúc câu trả lời. |

Nên biết:

- **Số trực tiếp là ước lượng.** Khi cuộc gọi đang stream, số token được ước lượng từ lượng byte đang về. Bản ghi hoàn chỉnh dùng số đếm chính xác của harness.
- **TTFT hơi khác nhau giữa các harness**, vì mỗi harness ghi lại một thứ khác nhau. OMP và Antigravity báo thời gian đến token *hiển thị* đầu tiên, nên ở đó phần thinking ẩn được tính vào độ trễ.
- **Token thinking được tính là đầu ra**, đúng như cách provider tính tiền. Tốc độ là tốc độ sinh của model, có thể cao hơn tốc độ chữ hiện trên màn hình.
- **Câu trả lời của Antigravity thường về thành một cụm.** Khi câu trả lời stream dưới một giây, thời gian stream không nói lên gì về model, nên tốc độ được tính bằng toàn bộ token đầu ra chia cho cả cuộc gọi.
- **Ô trống không phải là số 0.** Khi harness không ghi lại một thứ gì đó, app để trống.

## Quyền riêng tư

- Không có gì rời khỏi máy bạn. Không tài khoản, không telemetry, không dịch vụ mạng.
- Từ dữ liệu phiên, app chỉ lấy mốc thời gian, số token và tên model. Các file đó cũng chứa prompt và câu trả lời của bạn; app bỏ qua và không lưu lại gì trong số đó. App không bao giờ mở các file chứa thông tin đăng nhập.
- Việc phát hiện qua mạng chỉ đọc bộ đếm byte của từng kết nối. Lưu lượng không bị giải mã hay chặn.
- Lịch sử cuộc gọi là mỗi cuộc gọi một dòng trong `~/Library/Application Support/SpeedTracker/history.jsonl` (macOS) hoặc `%LOCALAPPDATA%\SpeedTracker` (Windows).

## Proxy tuỳ chọn

App cũng chạy một proxy ở `127.0.0.1:4141`. Trỏ base URL của một công cụ vào đó thì stream phản hồi được đo trực tiếp, với mọi provider. Không tính năng nào khác phụ thuộc vào nó.

```
http://127.0.0.1:4141/<route>/<phần còn lại của đường dẫn>
http://127.0.0.1:4141/_/<host https bất kỳ>/<phần còn lại của đường dẫn>
```

Các route: `anthropic`, `openai`, `chatgpt`, `deepseek`, `openrouter`, `gemini`, `xai`, `groq`, `cerebras`, `mistral`, `moonshot`, `zai`, `ollama`, `lmstudio`. Thêm `@tên` vào route để gắn nhãn công cụ, ví dụ `/anthropic@mytool/...`.

Proxy chỉ lắng nghe trên loopback, từ chối yêu cầu có header `Origin` của trình duyệt, và không proxy WebSocket.

## Windows

Bản Windows dùng chung quy tắc đo và định dạng lịch sử với macOS, nhưng là một bản cài đặt riêng viết bằng C#.

- **Không cần quyền quản trị**, app đọc dữ liệu phiên của harness, tức là đủ cho mọi harness trong bảng đầu tiên ở trên.
- **Đo thời gian qua mạng** cần bộ thu tuỳ chọn, và nó chỉ xin quyền quản trị khi bạn bật lên. Không bật thì các harness không có dữ liệu phiên sẽ không được đo.
- **Tình trạng: thử nghiệm.** Test đều qua và các gói được build trên CI, nhưng khay hệ thống, các cửa sổ và bộ thu chưa được kiểm tra bằng tay trên máy Windows. Rất mong bạn báo lại nếu gặp lỗi.

## Giới hạn

- Harness không có dữ liệu phiên chỉ được đo qua lưu lượng mạng, với số token ước lượng.
- Trên macOS, mỗi lần lấy mẫu bộ đếm mạng phải chạy công cụ hệ thống `nettop`: mỗi giây một lần khi có harness đang mở, tối đa mười lần mỗi giây khi một yêu cầu đang chờ byte đầu tiên.
- Các bản build chưa được ký hay notarize.

## Build từ mã nguồn

**macOS** cần Xcode Command Line Tools; không cần cài Xcode đầy đủ.

```bash
./Scripts/build_app.sh
```

```bash
./Scripts/test.sh
```

**Windows** cần .NET 8 SDK.

```powershell
.\Scripts\build_windows.ps1 -Runtime win-x64
```

```powershell
dotnet run --project Windows/SpeedTracker.Smoke/SpeedTracker.Smoke.csproj -c Release
```

Để xem app phát hiện được gì trên máy Mac:

```bash
"Speed Tracker.app/Contents/MacOS/SpeedTracker" --diagnose 30
```

GitHub Actions build và test cả hai app mỗi lần push. Push một tag như `v0.1.0` sẽ phát hành chúng thành một release.

## Cấu trúc dự án

```
Sources/SpeedTrackerCore   phát hiện, đọc phiên, theo dõi luồng mạng, lịch sử, phân tích dashboard (không có UI)
Sources/SpeedTracker       app thanh menu và dashboard cho macOS
Tests/                     test cho macOS
Windows/                   lõi Windows, app, bộ thu và proxy tuỳ chọn, test
Scripts/                   script build và test
AGENTS.md                  ghi chú cho người đóng góp và coding agent: định dạng, quyết định, cạm bẫy
```

## Ghi nhận

Ý tưởng về một ứng dụng nhỏ trên thanh menu đi kèm coding agent đến từ [CodexBar](https://github.com/steipete/CodexBar).

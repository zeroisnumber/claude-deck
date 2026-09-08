# WebGL 컨텍스트 손실 재현 장치

GPU 드라이버 리셋(WebView2 GPU 프로세스 재시작) 뒤 터미널이 빈 화면으로 남던 문제를
헤드리스 크롬에서 그대로 재현한다. 앱과 같은 xterm 번들을 쓰고, 글리프 아틀라스를 공유하는
터미널 세 개를 띄운 뒤 CDP `Browser.crashGpuProcess`로 GPU 프로세스를 실제로 죽인다.

```
pip install --target py websockets
python run.py py "C:\Program Files\Google\Chrome\Application\chrome.exe" old out_old.png
python run.py py "C:\Program Files\Google\Chrome\Application\chrome.exe" new out_new.png
```

- `mode=old`: 0.4.0의 처리 (onContextLoss에서 dispose + refresh). 크래시 뒤 세 터미널 모두 px=0.
- `mode=new`: `webglcontextrestored`를 받아 모든 탭의 애드온을 버리고 다시 만든다. 크래시 뒤에도 정상.

`px`는 캔버스 아래 40줄에서 배경이 아닌 픽셀 수(그리기 버퍼 보존 옵션으로 읽음), `domText`는
DOM 렌더러가 그린 글자 수다. 앱의 `scheduleWebglRebuild`는 `mode=new`와 같은 로직이다.

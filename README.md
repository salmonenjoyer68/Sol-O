# Sol-O

Phase 1: one FastAPI endpoint (`POST /run`) that runs the researcher agent and returns validated JSON.

```bash
pip install -r requirements.txt
cp .env.example .env              # set BASE_URL, API_KEY, MODEL
uvicorn app.main:app --reload
python -m tests.test_run "your task here"
```

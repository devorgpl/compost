FROM python:3.13-slim
WORKDIR /app
RUN pip install --no-cache-dir "psycopg[binary]==3.2.*"
COPY app.py migrate.py ./
EXPOSE 5000
CMD ["python", "app.py"]

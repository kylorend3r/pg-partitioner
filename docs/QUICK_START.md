# Quick Start — postgresql-partitioner

**Get started in 5 minutes. All commands are safe — nothing writes to your database until you explicitly `apply` a plan.**

---

## ⚡ 60-Second Setup

```bash
# 1. Build the binary
cargo build --release

# 2. Set your database connection
export PG_HOST=localhost
export PG_PORT=5432
export PG_DATABASE=mydb
export PG_USER=postgres
export PG_PASSWORD=secret

# 3. Run your first (read-only) command
./target/release/pg-partitioner inspect

# Done! You just read your database's partitioning state without changing anything.
```

---

## 📋 Common Commands (Copy-Paste Ready)

### 1. See Current Partitioning
```bash
pg-partitioner inspect

# Optional: Filter to one table
pg-partitioner inspect --table events

# Optional: Get JSON instead of text
pg-partitioner inspect --format json > report.json
```

### 2. Understand Your Setup in Plain English
```bash
pg-partitioner explain
```

### 3. Health Check (Risk Signals)
```bash
pg-partitioner doctor

# Optional: Focus on one table
pg-partitioner doctor --table events
```

### 4. Plan a Partitioning Migration (No Database Changes Yet)
```bash
pg-partitioner plan \
  --schema public \
  --table users \
  --output migration-plan.json

# Review the plan
cat migration-plan.json | jq '.'
```

### 5. Actually Apply the Plan (Atomic, Safe)
```bash
# First, dry-run (show what would happen)
pg-partitioner apply --plan-file migration-plan.json --dry-run

# Ready? Apply for real
pg-partitioner apply --plan-file migration-plan.json
```

### 6. Automate Premake + Retention
```bash
# Run once manually to test
pg-partitioner maintain

# Add to cron for daily automation (Linux/Mac)
# (Open crontab editor)
crontab -e

# Add this line:
0 0 * * * cd /path/to/repo && ./target/release/pg-partitioner maintain >> maintenance.log 2>&1
```

### 7. Export Configuration to Git
```bash
pg-partitioner export --output partitioning-config.yaml

# Commit to version control
git add partitioning-config.yaml
git commit -m "Partition configuration as code"
```

### 8. Health Monitoring (Run Regularly)
```bash
# Daily health check
pg-partitioner doctor > /tmp/health-report.txt

# Check the report
cat /tmp/health-report.txt
```

### 9. Cost Projection (Storage Forecasting)
```bash
# "How big will this table be in 6 months?"
pg-partitioner advisor storage-projection \
  --table events \
  --growth-rate 100000 \
  --months 6
```

### 10. Get Strategy Recommendations
```bash
# "How should I partition this new table?"
pg-partitioner advisor recommend \
  --table new_table \
  --row-count 50000000 \
  --size-mb 5000
```

---

## 🔒 Safety Checklist

Before running `apply`:

- ✅ Have you reviewed the plan file?
- ✅ Is this a new table or tested on staging first?
- ✅ Do you have a backup?
- ✅ Is it off-peak hours (optional but safer)?
- ✅ Have you tested with `--dry-run`?

**What makes `apply` safe:**
- Drift detection: refuses to apply if schema changed
- Lock timeouts: won't hang or block production traffic
- Retry logic: automatic backoff on transient contention
- Audit logging: every action logged in `partitioner_logbook`
- Atomic cutover: table rename, parent rename, ATTACH all in <1 second

---

## 🐛 Troubleshooting

### "Connection failed"
```bash
# Verify credentials
export PG_HOST=your-host
export PG_PORT=5432
export PG_USER=your_user
export PG_PASSWORD=your_pass

# Test the connection
pg-partitioner inspect
```

### "Permission denied"
```bash
# You need at least SELECT on system catalogs
# For apply operations, need CREATE/ALTER/DROP on target schema

# Quick test:
pg-partitioner doctor  # Read-only, shows if you have SELECT permissions
```

### "Plan file is invalid"
```bash
# Regenerate the plan (it might be stale)
pg-partitioner plan --schema public --table mytable --output new-plan.json
pg-partitioner apply --plan-file new-plan.json --dry-run
```

### "Drift detected"
```bash
# Schema changed since plan was created
# Solution: Regenerate the plan
pg-partitioner plan --schema public --table mytable --output new-plan.json
pg-partitioner apply --plan-file new-plan.json
```

---

## 📊 Typical Workflow

```
Day 1: Discovery
  └─ pg-partitioner inspect          # What do we have?
     pg-partitioner doctor           # Any issues?
     pg-partitioner explain          # Explain to team

Day 2: Planning
  └─ pg-partitioner advisor recommend  # Strategy?
     pg-partitioner plan --output plan.json

Day 3: Staging Validation (Optional but Recommended)
  └─ (Switch to staging database)
     pg-partitioner apply --plan-file plan.json --dry-run
     pg-partitioner apply --plan-file plan.json

Day 4: Production Deployment
  └─ (Off-peak hours)
     pg-partitioner apply --plan-file plan.json --dry-run
     pg-partitioner apply --plan-file plan.json

Day 5+: Ongoing Operations
  └─ pg-partitioner maintain          # (Daily via cron)
     pg-partitioner doctor            # (Weekly health check)
     pg-partitioner advisor storage-projection  # (Monthly planning)
```

---

## 🎯 By Team Role

### **Database Administrator**
```bash
# Daily
pg-partitioner doctor --format json > health.json

# Weekly
pg-partitioner maintain --dry-run
pg-partitioner inspect

# Monthly
pg-partitioner advisor storage-projection --months 1
```

### **Application Developer**
```bash
# Understand impact of partitioning
pg-partitioner explain

# Before pushing large-table changes
pg-partitioner doctor --table my_table
```

### **DevOps/Automation**
```bash
# Add to infrastructure code
pg-partitioner maintain     # Scheduled daily
pg-partitioner doctor       # Scheduled weekly (alert if critical)
pg-partitioner export       # Store in git (version control)
```

### **Data Engineer (On-boarding new tables)**
```bash
# 1. Discover current state
pg-partitioner inspect

# 2. Get recommendation
pg-partitioner advisor recommend --table new_table

# 3. Plan
pg-partitioner plan --schema public --table new_table --output plan.json

# 4. Test
pg-partitioner apply --plan-file plan.json --dry-run

# 5. Apply
pg-partitioner apply --plan-file plan.json

# 6. Automate
pg-partitioner maintain  # Test it
# (Add to cron)
```

---

## 💡 Tips & Tricks

### Running Against Multiple Databases
```bash
# Database A
export PG_HOST=db-a.company.com
pg-partitioner inspect > /tmp/db-a-report.txt

# Database B
export PG_HOST=db-b.company.com
pg-partitioner inspect > /tmp/db-b-report.txt

# Compare
diff /tmp/db-a-report.txt /tmp/db-b-report.txt
```

### Exporting Configuration to Git
```bash
# Export from production
pg-partitioner export --output partitions.yaml

# Stage in git
git add partitions.yaml
git commit -m "Partition config snapshot as of 2026-07-19"

# Later, compare versions
git diff HEAD~1 partitions.yaml  # What changed?
```

### Scheduling Maintenance
```bash
# Via cron (Linux/Mac)
crontab -e
# Add: 0 0 * * * pg-partitioner maintain >> /var/log/partitioner.log 2>&1

# Via systemd (Linux)
# Create /etc/systemd/system/pg-partitioner.timer
# Create /etc/systemd/system/pg-partitioner.service
# systemctl start pg-partitioner.timer

# Via Kubernetes CronJob
# See deployment guides in /docs
```

### Integration with Monitoring
```bash
# Capture health check output
pg-partitioner doctor --format json > /tmp/health.json

# Parse and alert
if grep -q "Critical" /tmp/health.json; then
  # Send alert (PagerDuty, Slack, etc.)
fi
```

---

## 🔗 Next Steps

1. **Try it locally** — `pg-partitioner inspect` against your dev database
2. **Read full docs** — See `PRACTICAL_USE_CASES.md` for detailed scenarios
3. **Plan a real migration** — `pg-partitioner plan` for an actual table
4. **Deploy incrementally** — Start with read-only commands, then plan/apply
5. **Automate** — Add `pg-partitioner maintain` to your scheduler

---

## 📞 Getting Help

```bash
# Show available commands
pg-partitioner --help

# Help for a specific command
pg-partitioner inspect --help
pg-partitioner plan --help
pg-partitioner apply --help

# Increase verbosity for debugging
pg-partitioner --log-level debug inspect

# Save logs to file
pg-partitioner --log-file /tmp/debug.log doctor
```

---

**That's it. You're ready to manage PostgreSQL partitioning safely and intelligently.**
